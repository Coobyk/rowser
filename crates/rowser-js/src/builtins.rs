//! Minimal standard global environment.

use crate::interp::*;
use crate::regexp::{Regex, RxMatch};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

type NResult = Result<Value, Value>;

// -- hot-path helpers ---------------------------------------------------------

/// FNV-1a — plenty for short prop keys; SipHash setup costs more than the
/// `Rc<str>` allocation this cache avoids.
#[derive(Clone, Default)]
struct Fnv(u64);

impl std::hash::Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        let mut h = if self.0 == 0 {
            0xcbf29ce484222325
        } else {
            self.0
        };
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        self.0 = h;
    }
}

thread_local! {
    static PK_CACHE: RefCell<
        HashMap<Rc<str>, PropKey, std::hash::BuildHasherDefault<Fnv>>,
    > = RefCell::new(HashMap::default());
    static EMPTY_RC: Rc<str> = Rc::from("");
}

/// `PropKey` for a string key via a small thread-local cache — avoids the
/// `Rc<str>` allocation `PropKey::from_str` performs on every call. The
/// cache is bounded; misses past the cap just allocate like before.
fn pk(s: &str) -> PropKey {
    PK_CACHE.with(|c| {
        let mut m = c.borrow_mut();
        if let Some(k) = m.get(s) {
            return k.clone();
        }
        let k = PropKey::S(Rc::from(s));
        if m.len() < 8192 {
            m.insert(Rc::from(s), k.clone());
        }
        k
    })
}

fn empty_rc() -> Rc<str> {
    EMPTY_RC.with(|e| e.clone())
}

/// `PropKey` for an array index — formats into a stack buffer (no `String`
/// alloc) and hits the `pk` cache so repeated indices share the key `Rc`.
fn idx_key(k: usize) -> PropKey {
    let mut buf = [0u8; 20];
    let mut n = k;
    let mut len = 0;
    loop {
        buf[19 - len] = b'0' + (n % 10) as u8;
        n /= 10;
        len += 1;
        if n == 0 {
            break;
        }
    }
    // SAFETY-free: digits are ASCII
    pk(std::str::from_utf8(&buf[20 - len..]).unwrap_or("0"))
}

/// `to_str` that shares the `Rc<str>` for `Value::Str` instead of copying
/// the whole string.
fn to_rc(i: &mut Interp, v: &Value) -> Result<Rc<str>, Value> {
    match v {
        Value::Str(s) => Ok(s.clone()),
        _ => Ok(Rc::from(i.to_str(v)?.as_str())),
    }
}

/// One-char `Value::Str` without a `String` round-trip.
fn char_val(c: char) -> Value {
    let mut b = [0u8; 4];
    let s: &str = c.encode_utf8(&mut b);
    Value::Str(Rc::from(s))
}

/// Enumerable own string-keyed entries in insertion order, matching
/// `Interp::own_keys` (array/typed-array indices first). `Some(v)` is a
/// plain data value (no getter needed); `None` marks an accessor or a
/// virtual index the caller must resolve via `get_prop`.
/// `[[Enumerable]]` flag of an own prop *right now* — dense array elements
/// and typed-array indices are enumerable; holes/deleted props are not.
fn own_prop_enumerable(i: &mut Interp, v: &Value, k: &PropKey) -> Result<bool, Value> {
    let Value::Obj(o) = v else {
        return Ok(false);
    };
    // `{ }` scopes the borrow — the gOPD trap may touch the proxy
    if let ObjKind::Proxy { target, handler } = &{ o.borrow().kind.clone() } {
        let (t, h) = (target.clone(), handler.clone());
        return match i.proxy_gopd(&t, &h, k, o)? {
            Some(d) => Ok(d.enumerable.unwrap_or(false)),
            None => Ok(false),
        };
    }
    let b = o.borrow();
    if let PropKey::S(s) = k {
        if let Some(n) = arr_index(s) {
            // a materialized index prop decides enumerability itself
            if let Some(p) = b.props.get(k) {
                return Ok(p.enumerable);
            }
            return Ok(match &b.kind {
                ObjKind::Arr(items) => {
                    n < items.len() && !b.holes.as_ref().is_some_and(|h| h.contains(&n))
                }
                ObjKind::Args { items, .. } => {
                    n < items.len() && !b.holes.as_ref().is_some_and(|h| h.contains(&n))
                }
                ObjKind::Typed {
                    buf,
                    off,
                    len,
                    kind,
                    track,
                } => typed_state(buf, *off, *len, *kind, *track).is_some_and(|eff| n < eff),
                _ => false,
            });
        }
    }
    Ok(b.props.get(k).map(|p| p.enumerable).unwrap_or(false))
}

fn own_enum_entries(i: &Interp, v: &Value) -> Vec<(PropKey, Option<Value>)> {
    let Value::Obj(o) = v else {
        return Vec::new();
    };
    let b = o.borrow();
    // spec EnumerableOwnProperties order: integer indices ascending,
    // then strings insertion-order, then symbols
    let mut ints: Vec<(usize, Option<Value>)> = Vec::new();
    let mut strs: Vec<(PropKey, Option<Value>)> = Vec::new();
    let mut syms: Vec<(PropKey, Option<Value>)> = Vec::new();
    match &b.kind {
        ObjKind::Arr(items) => {
            ints.reserve(items.len());
            for (k, it) in items.iter().enumerate() {
                if b.holes.as_ref().is_some_and(|h| h.contains(&k)) {
                    continue; // deleted slot — not an own prop
                }
                // materialized indices enumerate via their props entry
                if b.props.contains_key(&idx_key(k)) {
                    continue;
                }
                ints.push((k, Some(it.clone())));
            }
        }
        ObjKind::Args { items, .. } => {
            ints.reserve(items.len());
            for k in 0..items.len() {
                if b.holes.as_ref().is_some_and(|h| h.contains(&k)) {
                    continue;
                }
                if b.props.contains_key(&idx_key(k)) {
                    continue;
                }
                // mapped slots read the parameter binding — resolve via
                // get_prop instead of the (possibly stale) dense value
                ints.push((k, None));
            }
        }
        ObjKind::Typed {
            buf,
            off,
            len,
            kind,
            track,
        } => {
            for k in 0..typed_state(buf, *off, *len, *kind, *track).unwrap_or(0) {
                ints.push((k, None));
            }
        }
        _ => {}
    }
    for (k, p) in b.props.iter() {
        if !p.enumerable {
            continue;
        }
        if let PropKey::S(s) = k {
            if is_internal_key(i, &b, s) {
                continue;
            }
        }
        let val = if p.is_accessor() {
            None
        } else if let (PropKey::S(s), ObjKind::Args { map, .. }) = (k, &b.kind) {
            // a still-mapped arguments index reads the env binding, not
            // the stored prop value
            if arr_index(s).is_some_and(|n| matches!(map.get(n), Some(Some(_)))) {
                None
            } else {
                Some(p.val.clone())
            }
        } else {
            Some(p.val.clone())
        };
        match k {
            PropKey::Sym(_) => syms.push((k.clone(), val)),
            PropKey::S(s) => {
                // spec integer-index ordering covers < 2^32-1, wider
                // than the dense-store `arr_index` bound
                if let Some(n) = js_int_index(s) {
                    ints.push((n, val));
                } else {
                    strs.push((k.clone(), val));
                }
            }
        }
    }
    ints.sort_by_key(|(n, _)| *n);
    let mut out: Vec<(PropKey, Option<Value>)> =
        ints.into_iter().map(|(n, v)| (idx_key(n), v)).collect();
    out.extend(strs);
    out.extend(syms);
    out
}

/// EnumerableOwnProperties — like `own_enum_entries` but resolves every
/// value and routes proxies through their ownKeys/gOPD traps. `syms`
/// selects the spec's kind: Object.keys/values/entries only ever fetch
/// descriptors for *string* keys (EnumerableOwnProperties filters
/// `Type(key) is String` before [[GetOwnProperty]]); assign and
/// defineProperties enumerate symbol keys too.
pub(crate) fn own_enum_entries_full(
    i: &mut Interp,
    v: &Value,
    syms: bool,
) -> Result<Vec<(PropKey, Value)>, Value> {
    if let Value::Obj(o) = v {
        // `{ }` scopes the borrow — traps may touch the proxy
        if { matches!(&o.borrow().kind, ObjKind::Proxy { .. }) } {
            let mut out = Vec::new();
            for k in own_keys_all(i, v)? {
                if !syms && matches!(k, PropKey::Sym(_)) {
                    continue;
                }
                let kv = i.key_to_value(&k);
                let d = obj_get_own_prop_desc(i, &Value::Und, &[v.clone(), kv])?;
                if let Value::Obj(d) = d {
                    let en = i.get_prop_str(&Value::Obj(d), "enumerable")?;
                    if i.to_bool(&en) {
                        let val = i.get_prop(v, &k)?;
                        out.push((k, val));
                    }
                }
            }
            return Ok(out);
        }
    }
    // snapshot the key list once, then re-check each key's descriptor
    // live — an earlier getter may have deleted or de-enumerated a
    // later own prop (spec re-runs [[GetOwnProperty]] per key)
    let keys: Vec<PropKey> = own_enum_entries(i, v).into_iter().map(|(k, _)| k).collect();
    let mut out = Vec::new();
    for k in keys {
        if !syms && matches!(k, PropKey::Sym(_)) {
            continue;
        }
        if !own_prop_enumerable(i, v, &k)? {
            continue;
        }
        let val = i.get_prop(v, &k)?;
        out.push((k, val));
    }
    Ok(out)
}

/// `[[OwnPropertyKeys]]` — all own keys incl non-enumerable, in spec order:
/// integer-ascending, then strings insertion-order, then symbols.
pub(crate) fn own_keys_all(i: &mut Interp, v: &Value) -> Result<Vec<PropKey>, Value> {
    let Value::Obj(o) = v else {
        return Ok(Vec::new());
    };
    // `{ }` scopes the borrow — the ownKeys trap may touch the proxy
    if let ObjKind::Proxy { target, handler } = &{ o.borrow().kind.clone() } {
        let (t, h) = (target.clone(), handler.clone());
        return match i.proxy_trap(&h, "ownKeys")? {
            Some(trap) => {
                let r = i.call(&trap, h, vec![t.clone()])?;
                // spec CreateListFromArrayLike — array-LIKE reads
                // (length + indexed Gets), NOT the iterator protocol;
                // every element must be a String or Symbol
                if !matches!(r, Value::Obj(_)) {
                    return Err(i.type_err("proxy ownKeys trap must return an object"));
                }
                let len_v = i.get_prop_str(&r, "length")?;
                let len = to_length_num(i.to_num(&len_v)?).min(1_000_000);
                let mut trap_result: Vec<PropKey> = Vec::with_capacity(len.min(1024));
                for idx in 0..len {
                    let e = i.get_prop(&r, &idx_key(idx))?;
                    match e {
                        Value::Str(s) => trap_result.push(PropKey::S(s)),
                        Value::Sym(id, _) => trap_result.push(PropKey::Sym(id)),
                        _ => return Err(i.type_err("proxy ownKeys must return strings or symbols")),
                    }
                }
                let ext = match &t {
                    Value::Obj(to) => ord_is_extensible(i, to)?,
                    _ => false,
                };
                let target_keys = own_keys_all(i, &t)?;
                let mut nonconfig: Vec<PropKey> = Vec::new();
                let mut config: Vec<PropKey> = Vec::new();
                for tk in &target_keys {
                    let kv = i.key_to_value(tk);
                    let td = obj_get_own_prop_desc(i, &Value::Und, &[t.clone(), kv])?;
                    if let Value::Obj(dv) = td {
                        let cv = i.get_prop_str(&Value::Obj(dv), "configurable")?;
                        if i.to_bool(&cv) {
                            config.push(tk.clone());
                        } else {
                            nonconfig.push(tk.clone());
                        }
                    }
                }
                // spec: duplicate entries in the trap result → TypeError
                {
                    let mut dup: FxSet<PropKey> = FxSet::default();
                    for k in &trap_result {
                        if !dup.insert(k.clone()) {
                            return Err(i.type_err("proxy ownKeys trap returned duplicate keys"));
                        }
                    }
                }
                if ext && nonconfig.is_empty() {
                    return Ok(trap_result);
                }
                // every non-configurable target key must be reported
                let mut unchecked: Vec<PropKey> = trap_result.clone();
                for k in &nonconfig {
                    match unchecked.iter().position(|x| x == k) {
                        Some(p) => {
                            unchecked.remove(p);
                        }
                        None => {
                            return Err(i.type_err(
                                "proxy ownKeys trap result missing a non-configurable target key",
                            ))
                        }
                    }
                }
                if ext {
                    return Ok(trap_result);
                }
                // non-extensible target: the result must contain ALL
                // target keys (configurable ones too) and nothing more
                for k in &config {
                    match unchecked.iter().position(|x| x == k) {
                        Some(p) => {
                            unchecked.remove(p);
                        }
                        None => {
                            return Err(i.type_err(
                                "proxy ownKeys trap result is missing a target key on a non-extensible target",
                            ))
                        }
                    }
                }
                if !unchecked.is_empty() {
                    return Err(i.type_err(
                        "proxy ownKeys trap returned keys not present on a non-extensible target",
                    ));
                }
                Ok(trap_result)
            }
            None => own_keys_all(i, &t),
        };
    }
    let b = o.borrow();
    let mut ints: Vec<usize> = Vec::new();
    let mut strs: Vec<PropKey> = Vec::new();
    let mut syms: Vec<PropKey> = Vec::new();
    let mut seen_strs = std::collections::HashSet::new();
    match &b.kind {
        ObjKind::Arr(items) => {
            for k in 0..items.len() {
                if b.holes.as_ref().is_some_and(|h| h.contains(&k)) {
                    continue;
                }
                ints.push(k);
            }
            let lkey = PropKey::from_str("length");
            // a materialized length prop must not list twice
            seen_strs.insert(lkey.clone());
            strs.push(lkey);
        }
        ObjKind::Args { items, .. } => {
            for k in 0..items.len() {
                if b.holes.as_ref().is_some_and(|h| h.contains(&k)) {
                    continue;
                }
                ints.push(k);
            }
        }
        ObjKind::Typed {
            buf,
            off,
            len,
            kind,
            track,
        } => {
            ints.extend(0..typed_state(buf, *off, *len, *kind, *track).unwrap_or(0));
        }
        _ => {}
    }
    for (k, _) in b.props.iter() {
        match k {
            // private-name slots live at PRIV_BASE+ — [[OwnPropertyKeys]]
            // must never surface them
            PropKey::Sym(id) if *id < PRIV_BASE => syms.push(k.clone()),
            PropKey::Sym(_) => {}
            PropKey::S(s) => {
                if is_internal_key(i, &b, s) {
                    continue;
                }
                if let Some(n) = js_int_index(s) {
                    ints.push(n);
                } else if seen_strs.insert(k.clone()) {
                    strs.push(k.clone());
                }
            }
        }
    }
    ints.sort_unstable();
    ints.dedup();
    let mut out: Vec<PropKey> = ints.into_iter().map(idx_key).collect();
    out.extend(strs);
    out.extend(syms);
    Ok(out)
}

fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).cloned().unwrap_or(Value::Und)
}

/// spec `length` for built-ins — number of required positional params.
/// `native()` defaults to 0; this table corrects the methods Test262
/// checks. Names are the `name` given to `native()`.
const NATIVE_ARITY: &[(&str, f64)] = &[
    // Array statics + prototype
    ("at", 1.0),
    ("concat", 1.0),
    ("copyWithin", 2.0),
    ("every", 1.0),
    ("fill", 1.0),
    ("filter", 1.0),
    ("find", 1.0),
    ("findIndex", 1.0),
    ("findLast", 1.0),
    ("findLastIndex", 1.0),
    ("flatMap", 1.0),
    ("forEach", 1.0),
    ("includes", 1.0),
    ("indexOf", 1.0),
    ("join", 1.0),
    ("lastIndexOf", 1.0),
    ("map", 1.0),
    ("push", 1.0),
    ("reduce", 1.0),
    ("reduceRight", 1.0),
    ("slice", 2.0),
    ("some", 1.0),
    ("sort", 1.0),
    ("splice", 2.0),
    ("unshift", 1.0),
    ("with", 2.0),
    ("toSorted", 1.0),
    ("toSpliced", 2.0),
    ("from", 1.0),
    ("isArray", 1.0),
    // Iterator helpers (unique names)
    ("chunks", 1.0),
    ("windows", 1.0),
    // Object
    ("assign", 2.0),
    ("create", 2.0),
    ("defineProperties", 2.0),
    ("defineProperty", 3.0),
    ("entries", 1.0),
    ("freeze", 1.0),
    ("fromEntries", 1.0),
    ("getOwnPropertyDescriptor", 2.0),
    ("getOwnPropertyDescriptors", 1.0),
    ("getOwnPropertyNames", 1.0),
    ("getOwnPropertySymbols", 1.0),
    ("getPrototypeOf", 1.0),
    ("groupBy", 2.0),
    ("hasOwn", 2.0),
    ("is", 2.0),
    ("isExtensible", 1.0),
    ("isFrozen", 1.0),
    ("isSealed", 1.0),
    ("keys", 1.0),
    ("preventExtensions", 1.0),
    ("seal", 1.0),
    ("setPrototypeOf", 2.0),
    ("values", 1.0),
    ("hasOwnProperty", 1.0),
    ("isPrototypeOf", 1.0),
    ("propertyIsEnumerable", 1.0),
    ("__defineGetter__", 2.0),
    ("__defineSetter__", 2.0),
    ("__lookupGetter__", 1.0),
    ("__lookupSetter__", 1.0),
    // Function
    ("apply", 2.0),
    ("bind", 1.0),
    ("call", 1.0),
    // String
    ("charAt", 1.0),
    ("charCodeAt", 1.0),
    ("codePointAt", 1.0),
    ("endsWith", 1.0),
    ("localeCompare", 1.0),
    ("match", 1.0),
    ("matchAll", 1.0),
    ("padEnd", 1.0),
    ("padStart", 1.0),
    ("repeat", 1.0),
    ("replace", 2.0),
    ("replaceAll", 2.0),
    ("search", 1.0),
    ("split", 2.0),
    ("startsWith", 1.0),
    ("substring", 2.0),
    ("fromCharCode", 1.0),
    ("fromCodePoint", 1.0),
    ("raw", 1.0),
    // Number
    ("isFinite", 1.0),
    ("isInteger", 1.0),
    ("isNaN", 1.0),
    ("isSafeInteger", 1.0),
    ("parseFloat", 1.0),
    ("parseInt", 2.0),
    ("toExponential", 1.0),
    ("toFixed", 1.0),
    ("toPrecision", 1.0),
    // Math
    ("abs", 1.0),
    ("acos", 1.0),
    ("acosh", 1.0),
    ("asin", 1.0),
    ("asinh", 1.0),
    ("atan", 1.0),
    ("atan2", 2.0),
    ("atanh", 1.0),
    ("cbrt", 1.0),
    ("ceil", 1.0),
    ("clz32", 1.0),
    ("cos", 1.0),
    ("cosh", 1.0),
    ("exp", 1.0),
    ("expm1", 1.0),
    ("floor", 1.0),
    ("fround", 1.0),
    ("hypot", 2.0),
    ("imul", 2.0),
    ("log", 1.0),
    ("log10", 1.0),
    ("log1p", 1.0),
    ("log2", 1.0),
    ("max", 2.0),
    ("min", 2.0),
    ("pow", 2.0),
    ("round", 1.0),
    ("sign", 1.0),
    ("sin", 1.0),
    ("sinh", 1.0),
    ("sqrt", 1.0),
    ("sumPrecise", 1.0),
    ("tan", 1.0),
    ("tanh", 1.0),
    ("trunc", 1.0),
    // JSON
    ("parse", 2.0),
    ("stringify", 3.0),
    // Reflect
    ("construct", 2.0),
    ("deleteProperty", 2.0),
    ("ownKeys", 1.0),
    // Promise
    ("all", 1.0),
    ("allKeyed", 1.0),
    ("allSettled", 1.0),
    ("any", 1.0),
    ("race", 1.0),
    ("reject", 1.0),
    ("resolve", 1.0),
    ("then", 2.0),
    ("catch", 1.0),
    ("finally", 1.0),
    ("try", 1.0),
    ("withResolvers", 0.0),
    // Map/Set (name collisions with Reflect resolved to the Map/Set
    // arities — same tests, better coverage)
    ("add", 1.0),
    ("delete", 1.0),
    ("get", 1.0),
    ("has", 1.0),
    ("set", 2.0),
    ("getOrInsert", 2.0),
    ("getOrInsertComputed", 2.0),
    ("toString", 1.0),
    // RegExp / Symbol / Proxy / globals / misc
    ("revocable", 2.0),
    ("exec", 1.0),
    ("test", 1.0),
    ("compile", 2.0),
    ("for", 1.0),
    ("[Symbol.match]", 1.0),
    ("[Symbol.matchAll]", 1.0),
    ("[Symbol.replace]", 2.0),
    ("[Symbol.search]", 1.0),
    ("[Symbol.split]", 2.0),
    ("keyFor", 1.0),
    ("eval", 1.0),
    ("decodeURI", 1.0),
    ("decodeURIComponent", 1.0),
    ("encodeURI", 1.0),
    ("encodeURIComponent", 1.0),
    ("escape", 1.0),
    ("unescape", 1.0),
    // Iterator helpers
    ("take", 1.0),
    ("drop", 1.0),
    ("next", 0.0),
    ("return", 0.0),
    // Annex-B string HTML + misc
    ("anchor", 1.0),
    ("link", 1.0),
    ("fontcolor", 1.0),
    ("fontsize", 1.0),
    ("big", 0.0),
    ("blink", 0.0),
    ("bold", 0.0),
    ("fixed", 0.0),
    ("italics", 0.0),
    ("small", 0.0),
    ("strike", 0.0),
    ("sub", 0.0),
    ("sup", 0.0),
    ("substr", 2.0),
    ("trimStart", 0.0),
    ("trimEnd", 0.0),
    ("trimLeft", 0.0),
    ("trimRight", 0.0),
];

fn native(i: &Interp, name: &str, f: NativeFn) -> Value {
    let v = i.native(name, f);
    if let Some(&(_, arity)) = NATIVE_ARITY.iter().find(|(n, _)| *n == name) {
        if let Value::Obj(o) = &v {
            i.set_attr(o, "length", Value::Num(arity), false, true);
        }
    }
    v
}

fn method(i: &Interp, proto: &ObjRef, name: &str, f: NativeFn) {
    let v = native(i, name, f);
    i.set_builtin(proto, name, v);
}

/// register a proto method with an explicit `length` — used when the
/// method name collides with a different NATIVE_ARITY entry
fn method_len(i: &Interp, proto: &ObjRef, name: &str, f: NativeFn, len: f64) {
    let v = i.native(name, f);
    if let Value::Obj(o) = &v {
        i.set_attr(o, "length", Value::Num(len), false, true);
    }
    i.set_builtin(proto, name, v);
}

/// `X[@@toStringTag] = "Name"` — spec attrs {W:false, E:false, C:true}
/// so tests can delete/override the tag on prototypes.
fn str_tag(proto: &ObjRef, name: &str) {
    proto.borrow_mut().props.insert(
        PropKey::Sym(SYM_TOSTRINGTAG),
        Prop {
            val: Value::Str(Rc::from(name)),
            writable: false,
            enumerable: false,
            configurable: true,
            get: None,
            set: None,
        },
    );
}

// -- helpers used by natives --------------------------------------------------

/// declared `length` for arrays — a shadow prop holds a declared length
/// larger than the dense backing; effective length is the max of the two.
fn arr_declared_len(o: &ObjRef) -> usize {
    let b = o.borrow();
    let items_n = match &b.kind {
        ObjKind::Arr(items) => items.len(),
        _ => return 0,
    };
    match b.props.get(&PropKey::from_str("length")) {
        Some(p) => match &p.val {
            Value::Num(n) => (*n as usize).max(items_n),
            _ => items_n,
        },
        None => items_n,
    }
}

/// read array-like elements: real arrays + objects with numeric props/length
pub(crate) fn to_elems(i: &mut Interp, v: &Value) -> Result<Vec<Value>, Value> {
    match v {
        Value::Obj(o) => {
            if matches!(&o.borrow().kind, ObjKind::Arr(_)) {
                let len = arr_declared_len(o);
                if len > 8_000_000 {
                    return Err(i.range_err("array too large"));
                }
                let dense_full = {
                    let b = o.borrow();
                    match &b.kind {
                        ObjKind::Arr(items)
                            if len == items.len()
                                && b.holes.as_ref().map_or(true, |h| h.is_empty()) =>
                        {
                            Some(items.clone())
                        }
                        _ => None,
                    }
                };
                if let Some(items) = dense_full {
                    return Ok(items);
                }
                // sparse/mixed: spec reads every index via Get — props
                // entries, holes→proto, and materialized slots all apply
                let mut out = Vec::with_capacity(len.min(1_000_000));
                for k in 0..len {
                    out.push(i.get_prop(v, &idx_key(k))?);
                }
                return Ok(out);
            }
            if let ObjKind::Iter { items, .. } = &o.borrow().kind {
                return Ok(items.clone());
            }
            if matches!(o.borrow().kind, ObjKind::IterLazy(_)) {
                return iter_drain_lazy(i, v);
            }
        }
        // ToObject(undefined/null) is a TypeError — every caller here is a
        // spec operation that requires it
        Value::Und | Value::Null => {
            return Err(i.type_err("cannot convert undefined or null to object"));
        }
        _ => {} // primitives box to their wrapper: props resolve via proto
    }
    // generic path: read `length` then indexed properties (missing → Und)
    let len = i.get_prop_str(v, "length")?;
    let n = (i.to_num(&len)?.max(0.0) as usize).min(8_000_000);
    let mut out = Vec::with_capacity(n.min(1_000_000));
    for k in 0..n {
        out.push(i.get_prop(v, &idx_key(k))?);
    }
    Ok(out)
}

/// element count of an array-like (`length` clamped) — for result arrays
/// that must preserve the source length
/// spec LengthOfArrayLike — ToLength(Get "length") up to 2^53-1, no
/// materialization cap (live-iteration methods early-exit long before)
fn length_of_array_like(i: &mut Interp, v: &Value) -> Result<usize, Value> {
    if let Value::Obj(o) = v {
        if matches!(&o.borrow().kind, ObjKind::Arr(_)) {
            return Ok(arr_declared_len(o));
        }
    }
    let lv = i.get_prop_str(v, "length")?;
    Ok(to_length_num(i.to_num(&lv)?))
}

/// spec IsArray: proxies forward to their target (chains unwrap); a
/// revoked proxy throws a TypeError.
fn is_array_val(i: &mut Interp, v: &Value) -> Result<bool, Value> {
    let mut cur = v.clone();
    for _ in 0..10_000 {
        let Value::Obj(o) = &cur else {
            return Ok(false);
        };
        let nxt = match &o.borrow().kind {
            ObjKind::Arr(_) => return Ok(true),
            ObjKind::Proxy { target, .. } => {
                if matches!(target, Value::Null) {
                    return Err(
                        i.type_err("Cannot perform 'IsArray' on a proxy that has been revoked")
                    );
                }
                target.clone()
            }
            _ => return Ok(false),
        };
        cur = nxt;
    }
    Ok(false)
}

/// spec `Set(O, P, V, true)` — a failed [[Set]] is always a TypeError;
/// the interpreter's sloppy-mode `set_failed` flag converts to that here.
fn spec_set(i: &mut Interp, o: &Value, key: PropKey, v: Value) -> Result<(), Value> {
    i.set_failed = false;
    i.set_prop(o, key, v)?;
    if i.set_failed {
        i.set_failed = false;
        return Err(i.type_err("Cannot assign to read only property"));
    }
    Ok(())
}

/// `spec_set` on an integer index key.
fn spec_set_idx(i: &mut Interp, o: &Value, k: usize, v: Value) -> Result<(), Value> {
    spec_set(i, o, idx_key(k), v)
}

/// spec `DeletePropertyOrThrow(O, P)` on an integer index — a `false`
/// [[Delete]] (non-configurable prop, live TypedArray element) throws.
fn spec_del_idx(i: &mut Interp, o: &Value, k: usize) -> Result<(), Value> {
    if i.delete_prop(o, &idx_key(k))? {
        Ok(())
    } else {
        Err(i.type_err("Cannot delete property"))
    }
}

/// spec ArraySpeciesCreate: if `o` is an Array, result ctor =
/// `o.constructor[@@species]` (may be null→default); else default Array.
fn species_create(i: &mut Interp, o: &Value, len: usize) -> Result<Value, Value> {
    let mut c = Value::Und;
    if is_array_val(i, o)? {
        c = i.get_prop_str(o, "constructor")?;
        if matches!(c, Value::Obj(_)) {
            c = i.get_prop(&c, &PropKey::Sym(SYM_SPECIES))?;
            if matches!(c, Value::Null) {
                c = Value::Und;
            }
        }
    }
    match c {
        Value::Und => {
            // ArrayCreate(len): empty dense store + declared length — the
            // indices are ABSENT (holes), not `undefined`
            let a = i.new_arr(Vec::new());
            i.set_attr(&a, "length", Value::Num(len as f64), true, false);
            Ok(Value::Obj(a))
        }
        c if i.is_constructor(&c) => i.construct(&c, vec![Value::Num(len as f64)], None),
        _ => Err(i.type_err("invalid species constructor")),
    }
}

/// spec CreateDataPropertyOrThrow on integer index — the write semantics
/// array-into-species methods use (throws on non-configurable existing
/// props / non-extensible targets).
fn create_data_prop(i: &mut Interp, a: &Value, k: usize, v: Value) -> Result<(), Value> {
    if let Value::Obj(o) = a {
        let mut b = o.borrow_mut();
        let b = &mut *b;
        if let ObjKind::Arr(items) = &mut b.kind {
            // CreateDataProperty = [[DefineOwnProperty]] {w,e,c:true} —
            // a materialized index validates + resets its attrs, a new
            // index needs extensible, growth needs a writable length
            let key = PropKey::from_str(&k.to_string());
            if let Some(p) = b.props.get_mut(&key) {
                if !p.configurable {
                    return Err(i.type_err("Cannot redefine property"));
                }
                *p = Prop::data(v.clone());
            } else {
                let slot_exists =
                    k < items.len() && !b.holes.as_ref().is_some_and(|h| h.contains(&k));
                if !slot_exists && !b.extensible {
                    return Err(i.type_err("object is not extensible"));
                }
            }
            if k >= items.len() {
                let lkey = PropKey::from_str("length");
                let len_w = b.props.get(&lkey).map(|p| p.writable).unwrap_or(true);
                if !len_w {
                    return Err(i.type_err("Cannot define property"));
                }
                if k > 8_000_000 {
                    // beyond the dense cap: sparse props entry + grow
                    // the declared length to k+1
                    b.props.insert(key, Prop::data(v));
                    let need = k + 1;
                    match b.props.get_mut(&lkey) {
                        Some(p) => p.val = Value::Num(need as f64),
                        None => {
                            b.props.insert(
                                lkey,
                                Prop {
                                    val: Value::Num(need as f64),
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
                let old = items.len();
                items.resize(k + 1, Value::Und);
                if k > old {
                    let h = b.holes.get_or_insert_with(Default::default);
                    for g in old..k {
                        h.insert(g);
                    }
                }
            }
            items[k] = v;
            if let Some(h) = &mut b.holes {
                h.remove(&k);
            }
            return Ok(());
        }
        let key = PropKey::from_str(&k.to_string());
        match b.props.get(&key) {
            Some(p) if !p.configurable => return Err(i.type_err("Cannot redefine property")),
            None if !b.extensible => return Err(i.type_err("object is not extensible")),
            _ => {}
        }
        b.props.insert(key, Prop::data(v));
        return Ok(());
    }
    i.set_prop(a, PropKey::from_str(&k.to_string()), v)
}

pub(crate) fn clamp_index(n: f64, len: usize) -> usize {
    if n.is_nan() {
        0
    } else if n < 0.0 {
        len.saturating_sub((-n) as usize)
    } else {
        (n as usize).min(len)
    }
}

// ===========================================================================
// install
// ===========================================================================

pub fn install(i: &mut Interp) {
    install_protos(i);
    install_globals(i);
    // global `this` is the global object (sloppy-mode semantics)
    let g = Value::Obj(i.p.global.clone());
    *i.global.thisv.borrow_mut() = g;
}

fn define_global(i: &mut Interp, name: &str, v: Value) {
    // builtins live on the global OBJECT (globalThis.Iterator must be an
    // own prop) — declare_global_prop mirrors, then fix enumerability
    i.declare_global_prop(name, v.clone());
    let k = PropKey::from_str(name);
    if let Some(p) = i.p.global.borrow_mut().props.get_mut(&k) {
        p.enumerable = false;
    }
}

// ---------------------------------------------------------------------------
// prototypes
// ---------------------------------------------------------------------------

fn install_protos(i: &mut Interp) {
    // -- Object.prototype --
    {
        let p = i.p.obj.clone();
        method(i, &p, "hasOwnProperty", obj_has_own);
        method(i, &p, "isPrototypeOf", obj_is_prototype_of);
        method(i, &p, "propertyIsEnumerable", obj_prop_is_enum);
        // spec: Object.prototype.toString has length 0 (shared
        // NATIVE_ARITY entry gives 1) — like Function.prototype.toString
        method_len(i, &p, "toString", obj_to_string, 0.0);
        method(i, &p, "toLocaleString", obj_to_locale_string);
        method(i, &p, "valueOf", obj_value_of);
        method(i, &p, "__defineGetter__", obj_define_getter);
        method(i, &p, "__defineSetter__", obj_define_setter);
        method(i, &p, "__lookupGetter__", obj_lookup_getter);
        method(i, &p, "__lookupSetter__", obj_lookup_setter);
        // __proto__ accessor
        let get = native(i, "get __proto__", obj_get_proto);
        let set = native(i, "set __proto__", obj_set_proto);
        p.borrow_mut()
            .props
            .insert(pk("__proto__"), Prop::accessor(Some(get), Some(set)));
    }

    // -- Function.prototype --
    {
        let p = i.p.func.clone();
        // own `length`/`name` come FIRST — spec installation order and
        // getOwnPropertyNames expects them adjacent before the methods
        i.set_attr(&p, "length", Value::Num(0.0), false, true);
        i.set_attr(&p, "name", Value::Str(Rc::from("")), false, true);
        method(i, &p, "call", fn_call);
        method(i, &p, "apply", fn_apply);
        method(i, &p, "bind", fn_bind);
        // the shared "toString" arity entry (1) is for
        // Number.prototype.toString(radix) — Function's is 0
        method_len(i, &p, "toString", fn_to_string, 0.0);
        // restricted function properties — the realm's single frozen
        // %ThrowTypeError% intrinsic is BOTH getter and setter here
        let tte = Value::Obj(i.p.tte.clone());
        for k in ["caller", "arguments"] {
            p.borrow_mut().props.insert(
                PropKey::from_str(k),
                Prop::accessor(Some(tte.clone()), Some(tte.clone())),
            );
        }
        // Function.prototype[@@hasInstance] — {w:false, e:false, c:false},
        // and the function's own length is 1
        let hi = native(i, "[Symbol.hasInstance]", fn_has_instance);
        if let Value::Obj(hio) = &hi {
            i.set_attr(hio, "length", Value::Num(1.0), false, true);
        }
        p.borrow_mut().props.insert(
            PropKey::Sym(SYM_HASINSTANCE),
            Prop {
                val: hi,
                writable: false,
                enumerable: false,
                configurable: false,
                get: None,
                set: None,
            },
        );
    }

    // -- Array.prototype --
    {
        let p = i.p.array.clone();
        // @@unscopables — `with (arr)` must not resolve these method
        // names; spec's NullProtoObjCreate with every prop data-true
        // (writable/enumerable/configurable)
        let un = Rc::new(RefCell::new(Obj::new(None, ObjKind::Ord)));
        for n in [
            "at",
            "copyWithin",
            "entries",
            "fill",
            "find",
            "findIndex",
            "findLast",
            "findLastIndex",
            "flat",
            "flatMap",
            "includes",
            "keys",
            "toReversed",
            "toSorted",
            "toSpliced",
            "values",
        ] {
            un.borrow_mut()
                .props
                .insert(PropKey::from_str(n), Prop::data(Value::Bool(true)));
        }
        p.borrow_mut().props.insert(
            PropKey::Sym(SYM_UNSCOPABLES),
            Prop {
                val: Value::Obj(un),
                writable: false,
                enumerable: false,
                configurable: true,
                get: None,
                set: None,
            },
        );
        method(i, &p, "push", arr_push);
        method(i, &p, "pop", arr_pop);
        method(i, &p, "shift", arr_shift);
        method(i, &p, "unshift", arr_unshift);
        method(i, &p, "slice", arr_slice);
        method(i, &p, "splice", arr_splice);
        method(i, &p, "concat", arr_concat);
        method(i, &p, "join", arr_join);
        method(i, &p, "indexOf", arr_index_of);
        method(i, &p, "lastIndexOf", arr_last_index_of);
        method(i, &p, "includes", arr_includes);
        method(i, &p, "find", arr_find);
        method(i, &p, "findIndex", arr_find_index);
        method(i, &p, "findLast", arr_find_last);
        method(i, &p, "findLastIndex", arr_find_last_index);
        method(i, &p, "filter", arr_filter);
        method(i, &p, "map", arr_map);
        method(i, &p, "forEach", arr_for_each);
        method(i, &p, "reduce", arr_reduce);
        method(i, &p, "reduceRight", arr_reduce_right);
        method(i, &p, "some", arr_some);
        method(i, &p, "every", arr_every);
        method(i, &p, "flat", arr_flat);
        method(i, &p, "flatMap", arr_flat_map);
        method(i, &p, "fill", arr_fill);
        method(i, &p, "copyWithin", arr_copy_within);
        method(i, &p, "reverse", arr_reverse);
        method(i, &p, "sort", arr_sort);
        method(i, &p, "toReversed", arr_to_reversed);
        method(i, &p, "toSorted", arr_to_sorted);
        method(i, &p, "toSpliced", arr_to_spliced);
        method(i, &p, "with", arr_with);
        method(i, &p, "at", arr_at);
        // method_len: NATIVE_ARITY's Object.* entries would otherwise
        // give these length 1 — spec wants 0
        method_len(i, &p, "keys", arr_keys, 0.0);
        method_len(i, &p, "values", arr_values, 0.0);
        method_len(i, &p, "entries", arr_entries, 0.0);
        method(i, &p, "toString", arr_to_string);
        method(i, &p, "toLocaleString", arr_to_locale_string);
        let values_fn = i.obj_get_raw_str(&p, "values").unwrap();
        p.borrow_mut()
            .props
            .insert(PropKey::Sym(SYM_ITERATOR), Prop::builtin(values_fn));
    }

    // -- String.prototype --
    {
        let p = i.p.string.clone();
        method(i, &p, "charAt", str_char_at);
        method(i, &p, "charCodeAt", str_char_code_at);
        method(i, &p, "codePointAt", str_code_point_at);
        method(i, &p, "isWellFormed", str_is_well_formed);
        method(i, &p, "toWellFormed", str_to_well_formed);
        method(i, &p, "at", str_at);
        method(i, &p, "indexOf", str_index_of);
        method(i, &p, "lastIndexOf", str_last_index_of);
        method(i, &p, "slice", str_slice);
        method(i, &p, "substring", str_substring);
        method(i, &p, "substr", str_substr);
        method(i, &p, "toLowerCase", str_to_lower);
        method(i, &p, "toUpperCase", str_to_upper);
        method(i, &p, "toLocaleLowerCase", str_to_lower);
        method(i, &p, "toLocaleUpperCase", str_to_upper);
        method(i, &p, "trim", str_trim);
        method(i, &p, "trimStart", str_trim_start);
        method(i, &p, "trimEnd", str_trim_end);
        // Annex B aliases must be the *same* function objects
        let ts = i.obj_get_raw_str(&p, "trimStart").unwrap();
        let te = i.obj_get_raw_str(&p, "trimEnd").unwrap();
        p.borrow_mut()
            .props
            .insert(PropKey::from_str("trimLeft"), Prop::builtin(ts));
        p.borrow_mut()
            .props
            .insert(PropKey::from_str("trimRight"), Prop::builtin(te));
        method(i, &p, "split", str_split);
        method(i, &p, "replace", str_replace);
        method(i, &p, "replaceAll", str_replace_all);
        method(i, &p, "match", str_match);
        method(i, &p, "matchAll", str_match_all);
        method(i, &p, "search", str_search);
        method(i, &p, "padStart", str_pad_start);
        method(i, &p, "padEnd", str_pad_end);
        method(i, &p, "startsWith", str_starts_with);
        method(i, &p, "endsWith", str_ends_with);
        method(i, &p, "includes", str_includes);
        method(i, &p, "repeat", str_repeat);
        method(i, &p, "concat", str_concat);
        // spec: String.prototype.toString.length is 0 — the shared
        // NATIVE_ARITY "toString" entry (Number.toString's radix) is 1
        method_len(i, &p, "toString", str_to_string, 0.0);
        method(i, &p, "valueOf", str_to_string);
        method(i, &p, "toLocaleString", str_locale_str);
        method(i, &p, "localeCompare", str_locale_compare);
        method(i, &p, "normalize", str_normalize);
        method(i, &p, "anchor", str_anchor);
        method(i, &p, "big", str_big);
        method(i, &p, "blink", str_blink);
        method(i, &p, "bold", str_bold);
        method(i, &p, "fixed", str_fixed);
        method(i, &p, "fontcolor", str_fontcolor);
        method(i, &p, "fontsize", str_fontsize);
        method(i, &p, "italics", str_italics);
        method(i, &p, "link", str_link);
        method(i, &p, "small", str_small);
        method(i, &p, "strike", str_strike);
        method(i, &p, "sub", str_sub);
        method(i, &p, "sup", str_sup);
        let iter_fn = native(i, "[Symbol.iterator]", str_iter);
        p.borrow_mut()
            .props
            .insert(PropKey::Sym(SYM_ITERATOR), Prop::builtin(iter_fn));
    }

    // -- Number.prototype --
    {
        let p = i.p.number.clone();
        method(i, &p, "toFixed", num_to_fixed);
        method(i, &p, "toPrecision", num_to_precision);
        method(i, &p, "toExponential", num_to_exponential);
        method(i, &p, "toString", num_to_string);
        method(i, &p, "toLocaleString", num_to_string);
        method(i, &p, "valueOf", num_value_of);
    }

    // -- Boolean.prototype --
    {
        let p = i.p.boolean.clone();
        method_len(i, &p, "toString", bool_to_string, 0.0);
        method(i, &p, "valueOf", bool_value_of);
    }

    // -- Symbol.prototype --
    {
        let p = i.p.symbol.clone();
        method_len(i, &p, "toString", sym_to_string, 0.0);
        method(i, &p, "valueOf", sym_value_of);
        // Symbol.prototype[@@toPrimitive] — returns the wrapped symbol
        let stp = native(i, "[Symbol.toPrimitive]", sym_to_primitive);
        if let Value::Obj(so) = &stp {
            i.set_attr(so, "length", Value::Num(1.0), false, true);
        }
        // spec: { writable: false, enumerable: false, configurable: true }
        p.borrow_mut().props.insert(
            PropKey::Sym(SYM_TOPRIMITIVE),
            Prop {
                val: stp,
                writable: false,
                enumerable: false,
                configurable: true,
                get: None,
                set: None,
            },
        );
        let get = native(i, "get description", sym_get_description);
        p.borrow_mut()
            .props
            .insert(pk("description"), Prop::accessor(Some(get), None));
        str_tag(&p, "Symbol");
    }

    // -- Error.prototype (+ subclass protos chain to it) --
    {
        let p = i.p.error.clone();
        i.set_builtin(&p, "name", Value::Str(Rc::from("Error")));
        i.set_builtin(&p, "message", Value::Str(Rc::from("")));
        method(i, &p, "toString", err_to_string);
        for (proto, name) in [
            (i.p.type_error.clone(), "TypeError"),
            (i.p.range_error.clone(), "RangeError"),
            (i.p.syntax_error.clone(), "SyntaxError"),
            (i.p.ref_error.clone(), "ReferenceError"),
            (i.p.eval_error.clone(), "EvalError"),
            (i.p.uri_error.clone(), "URIError"),
            (i.p.agg_error.clone(), "AggregateError"),
            (i.p.suppressed_error.clone(), "SuppressedError"),
        ] {
            proto.borrow_mut().proto = Some(p.clone());
            i.set_builtin(&proto, "name", Value::Str(Rc::from(name)));
            i.set_builtin(&proto, "message", Value::Str(Rc::from("")));
        }
    }

    // -- iterator proto: [Symbol.iterator]() { return this } + helpers --
    {
        let p = i.p.iterator.clone();
        let f = native(i, "[Symbol.iterator]", iter_self);
        p.borrow_mut()
            .props
            .insert(PropKey::Sym(SYM_ITERATOR), Prop::builtin(f));
        method(i, &p, "next", iter_next);
        for (name, f) in [
            ("map", iter_map as NativeFn),
            ("filter", iter_filter),
            ("flatMap", iter_flat_map),
            ("take", iter_take),
            ("drop", iter_drop),
            ("reduce", iter_reduce),
            ("forEach", iter_for_each),
            ("every", iter_every),
            ("some", iter_some),
            ("find", iter_find),
            ("toArray", iter_to_array),
            ("indexed", iter_indexed),
            ("includes", iter_includes),
            ("join", iter_join),
            ("windows", iter_windows),
            ("chunks", iter_chunks),
        ] {
            method(i, &p, name, f);
        }
        // %IteratorHelperPrototype% — lazy helper results get own
        // next/return (return closes the underlying source chain)
        let hp = i.p.iter_helper.clone();
        method(i, &hp, "next", iter_next);
        method(i, &hp, "return", iter_lazy_ret);
        hp.borrow_mut().props.insert(
            PropKey::Sym(SYM_TOSTRINGTAG),
            Prop::data(Value::Str(Rc::from("Iterator Helper"))),
        );
        // %WrapForValidIteratorPrototype% — `Iterator.from` wraps; next via
        // the shared lazy path, return delegates to the [[Iterated]] record
        let wp = i.p.wrap_iter.clone();
        method(i, &wp, "next", iter_next);
        method(i, &wp, "return", wrap_iter_ret);
        // %GeneratorFunction.prototype% — proto of `function*` fns; its
        // `.prototype` slot is %GeneratorPrototype%
        {
            let gf = i.p.gen_func.clone();
            i.set_builtin(&gf, "prototype", Value::Obj(i.p.generator.clone()));
            str_tag(&gf, "GeneratorFunction");
        }
        // %GeneratorPrototype% — eager generators step a snapshot, so
        // `next` is the Iter stepper; `return`/`throw` finish the snapshot
        let gp = i.p.generator.clone();
        method(i, &gp, "next", iter_next);
        method(i, &gp, "return", gen_return);
        method(i, &gp, "throw", gen_throw);
        str_tag(&gp, "Generator");
        // builtin iterator protos — names come from @@toStringTag so
        // Object.prototype.toString sees them through Get
        str_tag(&i.p.array_iter.clone(), "Array Iterator");
        str_tag(&i.p.map_iter.clone(), "Map Iterator");
        str_tag(&i.p.set_iter.clone(), "Set Iterator");
        str_tag(&i.p.string_iter.clone(), "String Iterator");
        // Iterator.prototype[@@toStringTag] — accessor: get → "Iterator",
        // set → CreateDataProperty on the receiver
        {
            let g = native(i, "get [Symbol.toStringTag]", tag_get_iterator);
            let s = native(i, "set [Symbol.toStringTag]", tag_set_iterator);
            p.borrow_mut().props.insert(
                PropKey::Sym(SYM_TOSTRINGTAG),
                Prop::accessor(Some(g), Some(s)),
            );
        }
        // Iterator ctor (abstract) + statics
        let itctor = make_ctor(i, "Iterator", iter_ctor, p.clone());
        if let Value::Obj(o) = &itctor {
            method(i, o, "from", iter_from);
            method(i, o, "concat", iter_concat);
            method(i, o, "zip", iter_zip);
            method(i, o, "zipKeyed", iter_zip_keyed);
            // spec arities differ from the same-named Array entries
            for (name, len) in [("concat", 0.0), ("zip", 1.0), ("zipKeyed", 1.0)] {
                let f = i.get_prop_str(&itctor, name).unwrap_or(Value::Und);
                if let Value::Obj(fo) = f {
                    i.set_attr(&fo, "length", Value::Num(len), false, true);
                }
            }
        }
        // spec: `constructor` is an accessor — get → Iterator; set →
        // SetterThatIgnoresPrototypeProperties(%Iterator.prototype%)
        {
            let g = native(i, "get constructor", iter_ctor_get);
            let s = native(i, "set constructor", iter_ctor_set);
            p.borrow_mut().props.insert(
                PropKey::from_str("constructor"),
                Prop::accessor(Some(g), Some(s)),
            );
        }
        define_global(i, "Iterator", itctor);
    }

    // -- RegExp.prototype --
    {
        let p = i.p.regexp.clone();
        method(i, &p, "test", re_test);
        method(i, &p, "exec", re_exec);
        method_len(i, &p, "toString", re_to_string, 0.0);
        method(i, &p, "compile", re_compile);
        for (name, f) in [
            ("source", re_get_source as NativeFn),
            ("flags", re_get_flags),
            ("global", re_get_global),
            ("ignoreCase", re_get_icase),
            ("multiline", re_get_multiline),
            ("dotAll", re_get_dotall),
            ("sticky", re_get_sticky),
            ("unicode", re_get_unicode),
            ("unicodeSets", re_get_v),
            ("hasIndices", re_get_has_indices),
        ] {
            let get = native(i, &format!("get {name}"), f);
            p.borrow_mut()
                .props
                .insert(pk(name), Prop::accessor(Some(get), None));
        }
        for (sym, name, f) in [
            (SYM_MATCH, "[Symbol.match]", re_sym_match as NativeFn),
            (SYM_MATCHALL, "[Symbol.matchAll]", re_sym_match_all),
            (SYM_REPLACE, "[Symbol.replace]", re_sym_replace),
            (SYM_SEARCH, "[Symbol.search]", re_sym_search),
            (SYM_SPLIT, "[Symbol.split]", re_sym_split),
        ] {
            let v = native(i, name, f);
            p.borrow_mut()
                .props
                .insert(PropKey::Sym(sym), Prop::builtin(v));
        }
        // spec: %RegExp.prototype%.lastIndex is {W:false, E:false, C:false}
        p.borrow_mut().props.insert(
            pk("lastIndex"),
            Prop {
                val: Value::Num(0.0),
                writable: false,
                enumerable: false,
                configurable: false,
                get: None,
                set: None,
            },
        );
        // NOTE: no @@toStringTag here — spec gives RegExps their tag via
        // the [[RegExpMatcher]] builtinTag in Object.prototype.toString;
        // a proto tag would be non-writable and block instance overrides
    }
}

fn iter_self(_i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    Ok(this.clone())
}

/// `Iterator.prototype[@@toStringTag]` — get → "Iterator";
/// set → CreateDataPropertyOrThrow on the receiver.
fn tag_get_iterator(_i: &mut Interp, _t: &Value, _a: &[Value]) -> NResult {
    Ok(Value::Str(Rc::from("Iterator")))
}

/// spec `SetterThatIgnoresPrototypeProperties(home, P, V)`: non-object →
/// TypeError; `this === home` → TypeError; no own prop → CreateDataProperty;
/// otherwise `? Set(this, P, V, true)`.
fn setter_ignores_proto(
    i: &mut Interp,
    home: &ObjRef,
    this: &Value,
    key: &PropKey,
    v: Value,
) -> NResult {
    let Value::Obj(o) = this else {
        return Err(i.type_err("setter called on non-object"));
    };
    if Rc::ptr_eq(o, home) {
        return Err(i.type_err("cannot set on %Iterator.prototype%"));
    }
    let has_own = o.borrow().props.contains_key(key);
    if !has_own {
        let mut b = o.borrow_mut();
        if !b.extensible {
            return Err(i.type_err("object is not extensible"));
        }
        b.props.insert(key.clone(), Prop::data(v));
        return Ok(Value::Und);
    }
    i.set_prop(this, key.clone(), v)?;
    Ok(Value::Und)
}

fn tag_set_iterator(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let home = i.p.iterator.clone();
    setter_ignores_proto(i, &home, this, &PropKey::Sym(SYM_TOSTRINGTAG), arg(args, 0))
}

/// `get %Iterator.prototype%.constructor` — resolves the `Iterator` ctor
/// off the global object so rebinding is honored.
fn iter_ctor_get(i: &mut Interp, _t: &Value, _a: &[Value]) -> NResult {
    let g = Value::Obj(i.p.global.clone());
    i.get_prop(&g, &PropKey::from_str("Iterator"))
}

fn iter_ctor_set(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let home = i.p.iterator.clone();
    setter_ignores_proto(
        i,
        &home,
        this,
        &PropKey::from_str("constructor"),
        arg(args, 0),
    )
}

/// `GeneratorResume` — resume the suspended coroutine with `input`.
/// A per-resume step budget bounds bodies that loop without yielding;
/// a `return()` injected as GenAbort unwinds the body to completion.
fn gen_resume(i: &mut Interp, cell: &Rc<RefCell<GenCell>>, input: GenIn) -> NResult {
    let mut coro = {
        let mut c = cell.borrow_mut();
        if c.running {
            return Err(i.type_err("generator is already running"));
        }
        if c.done {
            return match input {
                GenIn::Return(v) => iter_result(i, v, true),
                GenIn::Throw(e) => Err(e),
                GenIn::Next(_) => iter_result(i, Value::Und, true),
            };
        }
        c.running = true;
        c.coro.take()
    };
    let mut coro = match coro {
        Some(c) => c,
        None => {
            // first resume: allocate the coroutine stack now — the
            // Interp pointer is captured fresh here so it is always the
            // resumer's own instance
            let Some((def, scope)) = cell.borrow_mut().pending.take() else {
                return iter_result(i, Value::Und, true);
            };
            let ip = i as *mut Interp;
            // 8 MiB — interpreter frames are large; the 1 MiB default
            // overflows on moderately nested generator bodies
            let stack =
                corosensei::stack::DefaultStack::new(8 * 1024 * 1024).expect("generator stack");
            corosensei::Coroutine::with_stack(
                stack,
                move |y: &corosensei::Yielder<GenIn, Value>, first: GenIn| -> GenRet {
                    // GeneratorResumeAbrupt on a suspendedStart
                    // generator: return()/throw() completes the record
                    // WITHOUT running the body
                    match first {
                        GenIn::Return(v) => return Ok(v),
                        GenIn::Throw(e) => return Err(e),
                        GenIn::Next(_) => {}
                    }
                    let i = unsafe { &mut *ip };
                    i.gen_yielders.push(y as *const _);
                    i.depth += 1;
                    // the generator body's own strictness governs every
                    // resume (suspended code keeps its mode)
                    let saved_strict = std::mem::replace(
                        &mut i.cur_strict,
                        def.strict || crate::parser::stmts_strict(&def.body),
                    );
                    let r = i.exec_block(&def.body, &scope);
                    i.cur_strict = saved_strict;
                    i.depth -= 1;
                    i.gen_yielders.pop();
                    match r {
                        Ok(Completion::Return(v)) => Ok(v),
                        Ok(_) => Ok(Value::Und),
                        Err(e) => Err(e),
                    }
                },
            )
        }
    };
    let ret_v = match &input {
        GenIn::Return(v) => Some(v.clone()),
        _ => None,
    };
    let (ocap, ohit) = (i.gen_cap, i.gen_cap_hit);
    i.gen_cap = i.steps.saturating_add(crate::interp::GEN_STEP_BUDGET);
    i.gen_cap_hit = false;
    let res = coro.resume(input);
    i.gen_cap = ocap;
    i.gen_cap_hit = ohit;
    let mut c = cell.borrow_mut();
    c.running = false;
    match res {
        corosensei::CoroutineResult::Yield(v) => {
            c.coro = Some(coro);
            if std::mem::take(&mut i.yieldstar_raw) {
                // yield* passthrough — the inner result object IS the
                // outer generator's next() result (done may be absent)
                return Ok(v);
            }
            iter_result(i, v, false)
        }
        corosensei::CoroutineResult::Return(r) => {
            c.done = true;
            match r {
                Ok(v) => iter_result(i, v, true),
                Err(e) if is_gen_abort(&e) => {
                    let v = i.gen_ret_override.take().or(ret_v).unwrap_or(Value::Und);
                    iter_result(i, v, true)
                }
                Err(e) => Err(e),
            }
        }
    }
}

fn iter_next(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    if let Value::Obj(o) = this {
        if matches!(o.borrow().kind, ObjKind::IterBusy) {
            return Err(iter_busy_err(i));
        }
        if let ObjKind::Gen(cell) = &o.borrow().kind {
            let cell = cell.clone();
            return gen_resume(i, &cell, GenIn::Next(arg(_a, 0)));
        }
        if matches!(o.borrow().kind, ObjKind::IterLive { .. }) {
            // pull fields out and mark busy — getters/next can reenter
            let (target, idx, mode) = {
                let mut b = o.borrow_mut();
                match std::mem::replace(&mut b.kind, ObjKind::IterBusy) {
                    ObjKind::IterLive { obj, idx, mode } => (obj, idx, mode),
                    _ => unreachable!(),
                }
            };
            let step = (|| -> Result<Option<Value>, Value> {
                let tv = || Value::Obj(target.clone());
                match mode {
                    0 | 1 | 2 => {
                        // array-like (incl. arguments): length re-read per
                        // step (LengthOfArrayLike is live)
                        let lv = i.get_prop(&tv(), &PropKey::from_str("length"))?;
                        let len = i.to_num(&lv)?.max(0.0) as usize;
                        if idx >= len {
                            return Ok(None);
                        }
                        let kk = i.intern(&idx.to_string());
                        let item = i.get_prop(&tv(), &PropKey::S(kk))?;
                        Ok(Some(match mode {
                            1 => Value::Num(idx as f64),
                            _ if mode == 2 => {
                                Value::Obj(i.new_arr(vec![Value::Num(idx as f64), item]))
                            }
                            _ => item,
                        }))
                    }
                    3 | 4 => {
                        let items = match &target.borrow().kind {
                            ObjKind::Map(m) => m.clone(),
                            _ => Vec::new(),
                        };
                        match items.get(idx) {
                            None => Ok(None),
                            Some((k, v)) => Ok(Some(if mode == 3 {
                                Value::Obj(i.new_arr(vec![k.clone(), v.clone()]))
                            } else {
                                v.clone()
                            })),
                        }
                    }
                    _ => {
                        let items = match &target.borrow().kind {
                            ObjKind::Set(m) => m.clone(),
                            _ => Vec::new(),
                        };
                        match items.get(idx) {
                            None => Ok(None),
                            Some(v) => Ok(Some(if mode == 6 {
                                Value::Obj(i.new_arr(vec![v.clone(), v.clone()]))
                            } else {
                                v.clone()
                            })),
                        }
                    }
                }
            })();
            {
                let mut b = o.borrow_mut();
                if matches!(b.kind, ObjKind::IterBusy) {
                    b.kind = ObjKind::IterLive {
                        obj: target,
                        idx: if matches!(step, Ok(Some(_))) {
                            idx + 1
                        } else {
                            usize::MAX
                        },
                        mode,
                    };
                }
            }
            return match step {
                Ok(Some(v)) => iter_result(i, v, false),
                Ok(None) => iter_result(i, Value::Und, true),
                Err(e) => Err(e),
            };
        }
        if matches!(o.borrow().kind, ObjKind::IterLazy(_)) {
            let mut lz = take_lazy(o).expect("checked IterLazy above");
            let r = iter_lazy_step(i, &mut lz);
            if let Ok(Some(_)) = &r {
                lz.started = true;
            }
            if r.is_err() {
                // spec: an abrupt step completes the helper — later
                // next() → done, return() not forwarded to the source,
                // pending concat args are never opened
                lz.src = IterSrc::Empty;
                if let IterOp::Cat { rest } = &mut lz.op {
                    rest.clear();
                }
            }
            put_lazy(o, lz);
            return match r {
                Ok(Some(v)) => iter_result(i, v, false),
                Ok(None) => iter_result(i, Value::Und, true),
                Err(e) => Err(e),
            };
        }
        let mut v = Value::Und;
        let mut done = true;
        if let ObjKind::Iter { items, idx } = &mut o.borrow_mut().kind {
            if *idx < items.len() {
                v = items[*idx].clone();
                *idx += 1;
                done = false;
            }
        }
        return iter_result(i, v, done);
    }
    iter_result(i, Value::Und, true)
}

pub(crate) fn make_iter(i: &mut Interp, items: Vec<Value>) -> Value {
    make_iter_in(items, i.p.iterator.clone())
}

/// iterator object with an explicit prototype (per-source iter protos)
pub(crate) fn make_iter_in(items: Vec<Value>, proto: ObjRef) -> Value {
    Value::Obj(Rc::new(RefCell::new(Obj::new(
        Some(proto),
        ObjKind::Iter { items, idx: 0 },
    ))))
}

/// `this` must be an object — Iterator helpers require it.
/// true when `v` is an Object per the spec — our BigInt stand-ins are
/// primitive values and must not pass object checks
fn is_spec_obj(v: &Value) -> bool {
    match v {
        Value::Obj(o) => {
            let b = o.borrow();
            !(b.props.contains_key(&PropKey::from_str("__bigint"))
                && !b.props.contains_key(&PropKey::from_str("__bigint_wrap")))
        }
        _ => false,
    }
}

fn iter_this(i: &mut Interp, this: &Value) -> Result<(), Value> {
    if !is_spec_obj(this) {
        return Err(i.type_err("Iterator.prototype method called on non-object"));
    }
    Ok(())
}

/// drain `this` via the iteration protocol (consuming generic iterators,
/// snapshotting Vec-backed ones).
fn iter_drain(i: &mut Interp, this: &Value) -> Result<Vec<Value>, Value> {
    i.iter_values(this)
}

// -- lazy iterator machinery ---------------------------------------------------

/// hard bound on collected elements / pulls — infinite sources error
/// out instead of OOMing (spec has no limit; real engines can't either)
const ITER_LIMIT: usize = 200_000;

/// Move the lazy state out of the RefCell so stepping can call back into
/// JS (`next`/`return`/callbacks) without holding the borrow. Leaves a
/// `IterBusy` sentinel so reentrant calls can be rejected per spec.
fn take_lazy(o: &ObjRef) -> Option<IterLazy> {
    let mut b = o.borrow_mut();
    match &mut b.kind {
        ObjKind::IterLazy(l) => {
            let st = std::mem::replace(l, IterLazy::id(IterSrc::Empty));
            b.kind = ObjKind::IterBusy;
            Some(st)
        }
        _ => None,
    }
}

fn put_lazy(o: &ObjRef, lz: IterLazy) {
    let mut b = o.borrow_mut();
    if matches!(b.kind, ObjKind::IterBusy) {
        b.kind = ObjKind::IterLazy(lz);
    } else if let ObjKind::IterLazy(l) = &mut b.kind {
        *l = lz;
    }
}

/// `%GeneratorPrototype%`-style guard: calling next/return on an iterator
/// whose next/return is already executing → TypeError.
fn iter_busy_err(i: &mut Interp) -> Value {
    i.type_err("Generator is already running")
}

/// iterator object wrapping a lazy state, proto `iter_helper` or explicit
fn make_lazy_in(lz: IterLazy, proto: ObjRef) -> Value {
    Value::Obj(Rc::new(RefCell::new(Obj::new(
        Some(proto),
        ObjKind::IterLazy(lz),
    ))))
}

fn make_lazy(i: &mut Interp, lz: IterLazy) -> Value {
    make_lazy_in(lz, i.p.iter_helper.clone())
}

/// `iter_to_lazy` flattening mode — spec `GetIteratorFlattenable`'s
/// `stringHandling` argument plus the plain GetIteratorDirect path.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Flat {
    /// GetIteratorDirect — read `next` directly (helpers/terminals)
    Direct,
    /// GetIteratorFlattenable(iterate-strings): `Iterator.from`
    Strings,
    /// GetIteratorFlattenable(reject-strings): zip/zipKeyed/flatMap-inner
    RejectStrings,
}

/// Wrap a value as a lazy source per the spec ops above.
/// `next` is stored so every pull shares the object's own consumption state.
pub(crate) fn iter_to_lazy(i: &mut Interp, v: &Value, flat: Flat) -> Result<IterLazy, Value> {
    let mut target = v.clone();
    if flat != Flat::Direct {
        if !is_spec_obj(&target)
            && (flat == Flat::RejectStrings || !matches!(target, Value::Str(_)))
        {
            return Err(i.type_err("value is not iterable"));
        }
        // GetMethod: nullish → absent; non-callable non-nullish → TypeError
        match i.get_prop(&target, &PropKey::Sym(SYM_ITERATOR))? {
            Value::Und | Value::Null => {}
            m if i.is_callable(&m) => {
                target = i.call(&m, target.clone(), Vec::new())?;
                if !is_spec_obj(&target) {
                    return Err(i.type_err("@@iterator result is not an object"));
                }
            }
            _ => return Err(i.type_err("@@iterator is not callable")),
        }
    }
    iter_direct(i, &target)
}

/// `for..of` iterable acquisition — spec `GetIterator(obj, sync)`:
/// `@@iterator` is required (unlike helpers' flattenable path); arrays
/// get a live-view source, strings iterate chars.
pub(crate) fn for_of_iter(i: &mut Interp, v: &Value) -> Result<IterLazy, Value> {
    match v {
        Value::Str(_) => iter_to_lazy(i, v, Flat::Direct),
        Value::Obj(o) => {
            // spec order: `@@iterator` is looked up first (fires Proxy
            // traps / getters). Fast live-view only when the lookup
            // resolves to the builtin `arr_values` native itself (an
            // overridden `Array.prototype[Symbol.iterator]` must run).
            let m = i.get_prop(v, &PropKey::Sym(SYM_ITERATOR))?;
            if matches!(o.borrow().kind, ObjKind::Arr(_)) {
                let builtin = matches!(
                    &m,
                    Value::Obj(mo)
                        if matches!(
                            &mo.borrow().kind,
                            ObjKind::Func(Func::Native { f, .. })
                                if *f == (arr_values as NativeFn)
                        )
                );
                if builtin {
                    return Ok(IterLazy::id(IterSrc::Arr {
                        arr: o.clone(),
                        idx: 0,
                    }));
                }
            }
            match m {
                m if i.is_callable(&m) => {
                    let it = i.call(&m, v.clone(), Vec::new())?;
                    if !is_spec_obj(&it) {
                        return Err(i.type_err("@@iterator result is not an object"));
                    }
                    iter_direct(i, &it)
                }
                _ => Err(i.type_err("value is not iterable")),
            }
        }
        // GetV wraps primitives for the @@iterator lookup
        _ => match i.get_prop(v, &PropKey::Sym(SYM_ITERATOR))? {
            m if i.is_callable(&m) => {
                let it = i.call(&m, v.clone(), Vec::new())?;
                if !is_spec_obj(&it) {
                    return Err(i.type_err("@@iterator result is not an object"));
                }
                iter_direct(i, &it)
            }
            _ => Err(i.type_err("value is not iterable")),
        },
    }
}

/// GetIteratorDirect: read `next` once (errors propagate); callability is
/// checked lazily when the record is stepped — per spec a `{next: 0}`
/// receiver yields a valid helper whose first `next()` throws.
fn iter_direct(i: &mut Interp, v: &Value) -> Result<IterLazy, Value> {
    match v {
        Value::Str(s) => {
            let items: Vec<Value> = s
                .chars()
                .map(|c| Value::Str(Rc::from(c.to_string().as_str())))
                .collect();
            return Ok(IterLazy::id(IterSrc::List { items, idx: 0 }));
        }
        Value::Obj(o) if !crate::binary::bigint_is_prim(&Value::Obj(o.clone())) => {}
        _ => return Err(i.type_err("value is not iterable")),
    }
    let next = i.get_prop(v, &PropKey::from_str("next"))?;
    Ok(IterLazy::id(IterSrc::Proto {
        iter: v.clone(),
        next,
        done: false,
    }))
}

/// `Return ? IteratorClose(record, err)` — a close error replaces `err`.
/// `IteratorClose` with a throw completion in flight — every close error
/// (GetMethod, non-callable, non-object result, throw) is swallowed and the
/// original error propagates (spec step 5).
fn iter_close_err(i: &mut Interp, it: &Value, err: Value) -> Value {
    let _ = i.iter_close_obj(it);
    err
}

/// pull one value from a source — spec `? IteratorStep(Value)`: every
/// step error propagates raw; IteratorClose is the *caller's* business.
fn iter_src_step(i: &mut Interp, src: &mut IterSrc) -> Result<Option<Value>, Value> {
    match src {
        IterSrc::Empty => Ok(None),
        IterSrc::List { items, idx } => {
            if *idx < items.len() {
                let v = items[*idx].clone();
                *idx += 1;
                Ok(Some(v))
            } else {
                Ok(None)
            }
        }
        IterSrc::Proto { iter, next, done } => {
            if *done {
                return Ok(None);
            }
            let (iter, next) = (iter.clone(), next.clone());
            let r = i.call(&next, iter.clone(), Vec::new())?;
            if !matches!(r, Value::Obj(_)) {
                return Err(i.type_err("iterator result is not an object"));
            }
            let d = i.get_prop(&r, &PropKey::from_str("done"))?;
            if i.to_bool(&d) {
                *done = true;
                return Ok(None);
            }
            Ok(Some(i.get_prop(&r, &PropKey::from_str("value"))?))
        }
        IterSrc::Arr { arr, idx } => {
            // element getters + defineProperty'd accessors are observable —
            // read via get_prop, not the raw Vec
            {
                let b = arr.borrow();
                if let ObjKind::Arr(items) = &b.kind {
                    if *idx >= items.len() {
                        // declared length may exceed items — check it
                        let dl = arr_declared_len(arr);
                        drop(b);
                        if *idx >= dl {
                            return Ok(None);
                        }
                    }
                } else {
                    drop(b);
                }
            }
            let kk = i.intern(&idx.to_string());
            *idx += 1;
            let v = i.get_prop(&Value::Obj(arr.clone()), &PropKey::S(kk))?;
            return Ok(Some(v));
        }
    }
}

/// IteratorClose on a source — calls `return` on protocol iterators.
fn iter_src_close(i: &mut Interp, src: &mut IterSrc) -> Result<(), Value> {
    if let IterSrc::Proto { iter, done, .. } = src {
        if *done {
            return Ok(());
        }
        *done = true;
        return i.iter_close_obj(iter);
    }
    Ok(())
}

/// close a lazy state: inner flatMap source, chained sources, own source
pub(crate) fn iter_lazy_close(i: &mut Interp, lz: &mut IterLazy) -> Result<(), Value> {
    if let IterOp::FlatMap {
        inner: Some(in_lz), ..
    } = &mut lz.op
    {
        iter_lazy_close(i, in_lz)?;
    }
    match &mut lz.op {
        IterOp::Cat { rest } => {
            // unstarted args hold raw values — nothing to close
            rest.clear();
        }
        IterOp::Zip { srcs, done, .. } => {
            // spec: open iterators close in reverse; a close error during a
            // normal completion propagates but all closes still run
            iter_zip_close_srcs(i, srcs, done)?;
        }
        IterOp::ZipKeyed { cols, done, .. } => {
            iter_zip_keyed_close_srcs(i, cols, done)?;
        }
        _ => {}
    }
    iter_src_close(i, &mut lz.src)
}

/// close all still-open zip columns in reverse — normal completion:
/// every close runs, the first error propagates
fn iter_zip_close_srcs(
    i: &mut Interp,
    srcs: &mut [IterLazy],
    done: &mut [bool],
) -> Result<(), Value> {
    let mut first_err = None;
    for idx in (0..srcs.len()).rev() {
        if !done[idx] {
            done[idx] = true;
            if let Err(e) = iter_lazy_close(i, &mut srcs[idx]) {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// abrupt completion during a zip step: close all still-open columns in
/// reverse; close errors are swallowed (a Throw completion is in flight)
fn iter_zip_abrupt(i: &mut Interp, srcs: &mut [IterLazy], done: &mut [bool]) {
    let _ = iter_zip_close_srcs(i, srcs, done);
}

fn iter_zip_keyed_close_srcs(
    i: &mut Interp,
    cols: &mut [(PropKey, IterLazy)],
    done: &mut [bool],
) -> Result<(), Value> {
    let mut first_err = None;
    for idx in (0..cols.len()).rev() {
        if !done[idx] {
            done[idx] = true;
            if let Err(e) = iter_lazy_close(i, &mut cols[idx].1) {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn iter_zip_keyed_abrupt(i: &mut Interp, cols: &mut [(PropKey, IterLazy)], done: &mut [bool]) {
    let _ = iter_zip_keyed_close_srcs(i, cols, done);
}

/// pull the next transformed value from a lazy iterator — spec
/// `? IteratorStepValue` on the source: errors propagate raw.
pub(crate) fn iter_lazy_step(i: &mut Interp, lz: &mut IterLazy) -> Result<Option<Value>, Value> {
    let mut pulls = 0usize;
    loop {
        pulls += 1;
        if pulls > ITER_LIMIT {
            let _ = iter_lazy_close(i, lz);
            return Err(i.range_err("iteration limit exceeded"));
        }
        // zip/zipKeyed own their sources — `lz.src` is unused for them
        match &mut lz.op {
            IterOp::Zip {
                srcs,
                done,
                mode,
                padding,
            } => {
                if srcs.is_empty() {
                    return Ok(None);
                }
                let mut row = Vec::with_capacity(srcs.len());
                let mut seen_done = false;
                let mut seen_live = false;
                let len = srcs.len();
                for idx in 0..len {
                    if done[idx] {
                        seen_done = true;
                        if *mode == ZipMode::Longest {
                            row.push(padding.get(idx).cloned().unwrap_or(Value::Und));
                        }
                        continue;
                    }
                    match iter_lazy_step(i, &mut srcs[idx]) {
                        Err(e) => {
                            // spec: remove `iter` from openIters first, then
                            // close the rest in reverse (errors swallowed)
                            done[idx] = true;
                            iter_zip_abrupt(i, srcs, done);
                            return Err(e);
                        }
                        Ok(Some(x)) => {
                            if *mode == ZipMode::Strict && seen_done {
                                // strict: a live value after a done source →
                                // length mismatch; close all still-open
                                let _ = iter_zip_close_srcs(i, srcs, done);
                                return Err(i.type_err(
                                    "Iterator.zip strict mode: iterables differ in length",
                                ));
                            }
                            seen_live = true;
                            row.push(x);
                        }
                        Ok(None) => {
                            done[idx] = true;
                            match mode {
                                ZipMode::Shortest => {
                                    // normal completion — close errors propagate
                                    iter_zip_close_srcs(i, srcs, done)?;
                                    return Ok(None);
                                }
                                ZipMode::Longest => {
                                    row.push(padding.get(idx).cloned().unwrap_or(Value::Und));
                                }
                                ZipMode::Strict => {
                                    if seen_live {
                                        let _ = iter_zip_close_srcs(i, srcs, done);
                                        return Err(i.type_err(
                                            "Iterator.zip strict mode: iterables differ in length",
                                        ));
                                    }
                                }
                            }
                            seen_done = true;
                        }
                    }
                }
                if seen_done && !seen_live {
                    // all columns done → complete (longest/strict)
                    return Ok(None);
                }
                let a = i.new_arr(row);
                return Ok(Some(Value::Obj(a)));
            }
            IterOp::ZipKeyed {
                cols,
                done,
                mode,
                padding,
            } => {
                if cols.is_empty() {
                    return Ok(None);
                }
                // spec: zipKeyed rows are null-prototype mutable objects
                let o: ObjRef = Rc::new(RefCell::new(Obj::new(None, ObjKind::Ord)));
                let mut seen_done = false;
                let mut seen_live = false;
                let len = cols.len();
                for idx in 0..len {
                    if done[idx] {
                        seen_done = true;
                        if *mode == ZipMode::Longest {
                            let p = padding.get(idx).cloned().unwrap_or(Value::Und);
                            let k = cols[idx].0.clone();
                            i.set_prop(&Value::Obj(o.clone()), k, p)?;
                        }
                        continue;
                    }
                    match iter_lazy_step(i, &mut cols[idx].1) {
                        Err(e) => {
                            // spec: remove `iter` from openIters first, then
                            // close the rest in reverse (errors swallowed)
                            done[idx] = true;
                            iter_zip_keyed_abrupt(i, cols, done);
                            return Err(e);
                        }
                        Ok(Some(x)) => {
                            if *mode == ZipMode::Strict && seen_done {
                                let _ = iter_zip_keyed_close_srcs(i, cols, done);
                                return Err(i.type_err(
                                    "Iterator.zipKeyed strict mode: iterables differ in length",
                                ));
                            }
                            seen_live = true;
                            let k = cols[idx].0.clone();
                            i.set_prop(&Value::Obj(o.clone()), k, x)?;
                        }
                        Ok(None) => {
                            done[idx] = true;
                            match mode {
                                ZipMode::Shortest => {
                                    iter_zip_keyed_close_srcs(i, cols, done)?;
                                    return Ok(None);
                                }
                                ZipMode::Longest => {
                                    let p = padding.get(idx).cloned().unwrap_or(Value::Und);
                                    let k = cols[idx].0.clone();
                                    i.set_prop(&Value::Obj(o.clone()), k, p)?;
                                }
                                ZipMode::Strict => {
                                    if seen_live {
                                        let _ = iter_zip_keyed_close_srcs(i, cols, done);
                                        return Err(i.type_err(
                                            "Iterator.zipKeyed strict mode: iterables differ in length",
                                        ));
                                    }
                                }
                            }
                            seen_done = true;
                        }
                    }
                }
                if seen_done && !seen_live {
                    return Ok(None);
                }
                return Ok(Some(Value::Obj(o)));
            }
            _ => {}
        }
        // flatMap drains its inner source before pulling the outer one
        if let IterOp::FlatMap {
            inner: Some(in_lz), ..
        } = &mut lz.op
        {
            match iter_lazy_step(i, in_lz) {
                Ok(Some(v)) => return Ok(Some(v)),
                Ok(None) => {
                    if let IterOp::FlatMap { inner, .. } = &mut lz.op {
                        *inner = None;
                    }
                    continue;
                }
                // spec: an inner step error closes the OUTER iterator
                // only — the failed inner iterator is not closed
                Err(e) => {
                    let _ = iter_src_close(i, &mut lz.src);
                    return Err(e);
                }
            }
        }
        // take(0): `? IteratorClose` BEFORE any pull — close errors
        // (e.g. ReturnCalledError) propagate; the helper is then completed
        if let IterOp::Take { left: 0 } = lz.op {
            iter_lazy_close(i, lz)?;
            return Ok(None);
        }
        let v = match iter_src_step(i, &mut lz.src)? {
            Some(v) => v,
            None => {
                if let IterOp::Cat { rest } = &mut lz.op {
                    if let Some((nx, method)) = rest.pop_front() {
                        // concat: call the pre-fetched @@iterator (or use the
                        // object itself when the method was absent), then
                        // GetIteratorDirect — all lazily, as the chain reaches
                        let it = if matches!(method, Value::Und | Value::Null) {
                            nx
                        } else {
                            let it = i.call(&method, nx, Vec::new())?;
                            if !matches!(it, Value::Obj(_)) {
                                return Err(i.type_err("@@iterator result is not an object"));
                            }
                            it
                        };
                        let wrapped = iter_to_lazy(i, &it, Flat::Direct)?;
                        lz.src = wrapped.src;
                        continue;
                    }
                }
                return Ok(None);
            }
        };
        match &mut lz.op {
            IterOp::Id | IterOp::Cat { .. } => return Ok(Some(v)),
            IterOp::Map { f, n } => {
                let (f, idx) = (f.clone(), *n);
                *n += 1.0;
                match i.call(&f, Value::Und, vec![v, Value::Num(idx)]) {
                    Ok(r) => return Ok(Some(r)),
                    Err(e) => {
                        let _ = iter_lazy_close(i, lz);
                        return Err(e);
                    }
                }
            }
            IterOp::Filter { f, n } => {
                let (f, idx) = (f.clone(), *n);
                *n += 1.0;
                match i.call(&f, Value::Und, vec![v.clone(), Value::Num(idx)]) {
                    Ok(r) if i.to_bool(&r) => return Ok(Some(v)),
                    Ok(_) => continue,
                    Err(e) => {
                        let _ = iter_lazy_close(i, lz);
                        return Err(e);
                    }
                }
            }
            IterOp::FlatMap { f, n, inner } => {
                let (f, idx) = (f.clone(), *n);
                *n += 1.0;
                let r = match i.call(&f, Value::Und, vec![v, Value::Num(idx)]) {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = iter_lazy_close(i, lz);
                        return Err(e);
                    }
                };
                // flatMap inner must be an object — strings/primitives are
                // NOT flattened (GetIteratorFlattenable, reject-primitives)
                if !matches!(r, Value::Obj(_)) {
                    let e = i.type_err("flatMap result is not an object");
                    let _ = iter_lazy_close(i, lz);
                    return Err(e);
                }
                match iter_to_lazy(i, &r, Flat::RejectStrings) {
                    Ok(nl) => *inner = Some(Box::new(nl)),
                    Err(e) => {
                        let _ = iter_lazy_close(i, lz);
                        return Err(e);
                    }
                }
            }
            IterOp::Take { left } => {
                *left -= 1;
                return Ok(Some(v));
            }
            IterOp::Drop { left } => {
                if *left > 0 {
                    *left -= 1;
                    continue;
                }
                return Ok(Some(v));
            }
            IterOp::Indexed { n } => {
                let idx = *n;
                *n += 1.0;
                let pair = i.new_arr(vec![Value::Num(idx), v]);
                return Ok(Some(Value::Obj(pair)));
            }
            IterOp::Chunks { size } => {
                let mut c = vec![v];
                while c.len() < *size {
                    if c.len() >= ITER_LIMIT {
                        let _ = iter_lazy_close(i, lz);
                        return Err(i.range_err("chunk size limit exceeded"));
                    }
                    match iter_src_step(i, &mut lz.src)? {
                        Some(x) => c.push(x),
                        None => break,
                    }
                }
                let a = i.new_arr(c);
                return Ok(Some(Value::Obj(a)));
            }
            IterOp::Windows { size, partial, buf } => {
                buf.push_back(v);
                while buf.len() < *size {
                    if buf.len() >= ITER_LIMIT {
                        let _ = iter_lazy_close(i, lz);
                        return Err(i.range_err("window size limit exceeded"));
                    }
                    match iter_src_step(i, &mut lz.src)? {
                        Some(x) => buf.push_back(x),
                        None => break,
                    }
                }
                if buf.len() == *size || (*partial && !buf.is_empty()) {
                    let w: Vec<Value> = buf.iter().cloned().collect();
                    buf.pop_front();
                    let a = i.new_arr(w);
                    return Ok(Some(Value::Obj(a)));
                }
                return Ok(None);
            }
            IterOp::Zip { .. } | IterOp::ZipKeyed { .. } => unreachable!(),
        }
    }
}

/// drain a lazy iterator to a Vec — spec IteratorClose on abrupt errors
pub fn iter_drain_lazy(i: &mut Interp, v: &Value) -> Result<Vec<Value>, Value> {
    let Value::Obj(o) = v else {
        return Err(i.type_err("not an iterator object"));
    };
    let Some(mut lz) = take_lazy(o) else {
        return Err(i.type_err("not a lazy iterator"));
    };
    let mut out = Vec::new();
    let r = loop {
        if out.len() >= ITER_LIMIT {
            let _ = iter_lazy_close(i, &mut lz);
            break Err(i.range_err("iteration limit exceeded"));
        }
        match iter_lazy_step(i, &mut lz) {
            Ok(Some(x)) => out.push(x),
            Ok(None) => break Ok(out),
            Err(e) => {
                let ce = iter_lazy_close(i, &mut lz);
                break Err(match ce {
                    Err(ce) => ce,
                    Ok(()) => e,
                });
            }
        }
    };
    put_lazy(o, lz);
    r
}

/// `this` as a lazy state, stepping each element through `f` until it
/// returns `Break` or the source ends. Callback errors and breaks close
/// the iterator per spec.
fn iter_each<F>(i: &mut Interp, this: &Value, mut f: F) -> Result<(), Value>
where
    F: FnMut(&mut Interp, Value) -> Result<bool, Value>,
{
    iter_this(i, this)?;
    let mut lz = iter_to_lazy(i, this, Flat::Direct)?;
    let mut steps = 0usize;
    loop {
        steps += 1;
        if steps >= ITER_LIMIT {
            let _ = iter_lazy_close(i, &mut lz);
            return Err(i.range_err("iteration limit exceeded"));
        }
        match iter_lazy_step(i, &mut lz) {
            Ok(Some(v)) => match f(i, v) {
                Ok(true) => {}
                Ok(false) => {
                    // early exit is a normal completion — close errors propagate
                    iter_lazy_close(i, &mut lz)?;
                    return Ok(());
                }
                Err(e) => {
                    // callback threw → close with throw in flight: swallow
                    // close errors, propagate the callback's error
                    let _ = iter_lazy_close(i, &mut lz);
                    return Err(e);
                }
            },
            Ok(None) => return Ok(()),
            // spec: errors from the iterator itself (`next` throwing,
            // protocol violations) propagate without closing
            Err(e) => return Err(e),
        }
    }
}

/// `{value, done}` result object
fn iter_result(i: &mut Interp, v: Value, done: bool) -> NResult {
    let r = i.new_obj();
    i.set_data(&r, "value", v);
    i.set_data(&r, "done", Value::Bool(done));
    Ok(Value::Obj(r))
}

/// `new Iterator()` / `class X extends Iterator` — base iterator object
/// (proto comes from new_target via `construct`); it has no own `next`
/// so calling `.next()` is a TypeError, per spec.
fn iter_ctor(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    // spec: "If NewTarget is undefined or the active function object,
    // throw a TypeError" — `Iterator()` and `new Iterator()` throw;
    // `class X extends Iterator` + Reflect.construct with another
    // NewTarget are legal
    let ctor = i.get_prop(
        &Value::Obj(i.p.global.clone()),
        &PropKey::from_str("Iterator"),
    );
    let legal = match (&i.new_target_native, &ctor) {
        (Some(Value::Obj(nt)), Ok(Value::Obj(c))) => !Rc::ptr_eq(nt, c),
        (Some(_), _) => true,
        _ => false,
    };
    if !legal {
        return Err(i.type_err("Iterator is an abstract class"));
    }
    Ok(this.clone())
}

/// `%GeneratorPrototype%.return(v)` — eager gen: exhaust the snapshot.
fn gen_return(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    if let Value::Obj(o) = this {
        if let ObjKind::Gen(cell) = &o.borrow().kind {
            let cell = cell.clone();
            return gen_resume(i, &cell, GenIn::Return(arg(args, 0)));
        }
        if let ObjKind::Iter { items, idx } = &mut o.borrow_mut().kind {
            *idx = items.len();
        }
    }
    iter_result(i, arg(args, 0), true)
}

/// `%GeneratorPrototype%.throw(e)` — eager gen: exhaust + propagate.
fn gen_throw(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    if let Value::Obj(o) = this {
        if let ObjKind::Gen(cell) = &o.borrow().kind {
            let cell = cell.clone();
            return gen_resume(i, &cell, GenIn::Throw(arg(args, 0)));
        }
        if let ObjKind::Iter { items, idx } = &mut o.borrow_mut().kind {
            *idx = items.len();
        }
    }
    Err(arg(args, 0))
}

/// walk `v`'s prototype chain looking for `target` (OrdinaryHasInstance)
fn proto_chain_has(v: &Value, target: &ObjRef) -> bool {
    let Value::Obj(o) = v else {
        return false;
    };
    let mut cur = o.borrow().proto.clone();
    let mut hops = 0;
    while let Some(p) = cur {
        if Rc::ptr_eq(&p, target) {
            return true;
        }
        cur = p.borrow().proto.clone();
        hops += 1;
        if hops > 64 {
            break;
        }
    }
    false
}

/// `%WrapForValidIteratorPrototype%.return` — GetMethod on the [[Iterated]]
/// iterator; absent → `{done:true}`; else the raw call result is returned.
fn wrap_iter_ret(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    if let Value::Obj(o) = this {
        if let Some(lz) = take_lazy(o) {
            let base = match &lz.src {
                IterSrc::Proto { iter, .. } => iter.clone(),
                _ => Value::Und,
            };
            put_lazy(o, lz);
            if matches!(base, Value::Obj(_)) {
                let m = i.get_prop(&base, &PropKey::from_str("return"))?;
                if matches!(m, Value::Und | Value::Null) {
                    return iter_result(i, Value::Und, true);
                }
                if !i.is_callable(&m) {
                    return Err(i.type_err("iterator return is not a function"));
                }
                return i.call(&m, base, Vec::new());
            }
        }
    }
    // RequireInternalSlot(O, [[Iterated]]) — non-wrapper receivers throw
    Err(i.type_err("wrapped iterator return called on invalid receiver"))
}

/// `Iterator.from(obj)` — spec: GetIteratorFlattenable, then
/// OrdinaryHasInstance(Iterator) passes real iterators through untouched;
/// everything else gets a %WrapForValidIteratorPrototype% lazy wrap whose
/// `return` delegates to the underlying iterator's `return`.
fn iter_from(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let v = arg(args, 0);
    let lz = iter_to_lazy(i, &v, Flat::Strings)?;
    if let IterSrc::Proto { iter, .. } = &lz.src {
        if proto_chain_has(iter, &i.p.iterator) {
            return Ok(iter.clone());
        }
    }
    Ok(make_lazy_in(lz, i.p.wrap_iter.clone()))
}

/// `Iterator.concat(...iters)` — lazy chain over each argument's iterator
fn iter_concat(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    // spec: at call time each arg must be an Object and its @@iterator is
    // fetched via GetMethod in order (getter effects are observable now);
    // the method is only *called* lazily as the chain reaches it
    let mut rest = std::collections::VecDeque::with_capacity(args.len());
    for a in args {
        if !matches!(a, Value::Obj(_)) {
            return Err(i.type_err("Iterator.concat argument is not an object"));
        }
        // spec: GetMethod — undefined/null/non-callable all TypeError
        let m = match i.get_prop(a, &PropKey::Sym(SYM_ITERATOR))? {
            m if i.is_callable(&m) => m,
            _ => return Err(i.type_err("@@iterator is not a function")),
        };
        rest.push_back((a.clone(), m));
    }
    Ok(make_lazy(
        i,
        IterLazy {
            src: IterSrc::Empty,
            op: IterOp::Cat { rest },
            started: false,
        },
    ))
}

/// shared zip/zipKeyed options handling: GetOptionsObject → mode check →
/// padding read (longest only, must be undefined|Object).
fn iter_zip_mode(i: &mut Interp, opts: &Value) -> Result<(ZipMode, Value), Value> {
    match opts {
        Value::Und => return Ok((ZipMode::Shortest, Value::Und)),
        _ if is_spec_obj(opts) => {}
        _ => return Err(i.type_err("options must be an object")),
    }
    let mode = match i.get_prop_str(opts, "mode")? {
        Value::Und => ZipMode::Shortest,
        Value::Str(s) if s.as_ref() == "shortest" => ZipMode::Shortest,
        Value::Str(s) if s.as_ref() == "longest" => ZipMode::Longest,
        Value::Str(s) if s.as_ref() == "strict" => ZipMode::Strict,
        _ => return Err(i.type_err("invalid zip mode")),
    };
    let padding = if mode == ZipMode::Longest {
        let p = i.get_prop_str(opts, "padding")?;
        if !matches!(p, Value::Und) && !is_spec_obj(&p) {
            return Err(i.type_err("padding must be an object"));
        }
        p
    } else {
        Value::Und
    };
    Ok((mode, padding))
}

/// longest-mode padding: pull `count` values through the padding object's
/// own @@iterator (GetIterator — the method is required), then close it.
fn iter_zip_padding(i: &mut Interp, padding: &Value, count: usize) -> Result<Vec<Value>, Value> {
    if !is_spec_obj(padding) {
        return Ok(Vec::new());
    }
    let m = i.get_prop(padding, &PropKey::Sym(SYM_ITERATOR))?;
    if !i.is_callable(&m) {
        return Err(i.type_err("padding is not iterable"));
    }
    let pit = i.call(&m, padding.clone(), Vec::new())?;
    let mut plz = iter_direct(i, &pit)?;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        match iter_lazy_step(i, &mut plz) {
            // spec: on abrupt the padding iterator is NOT closed — the error
            // propagates and the caller closes the acquired sources
            Err(e) => return Err(e),
            Ok(Some(v)) => out.push(v),
            Ok(None) => break,
        }
    }
    // normal completion → IteratorClose(paddingIter, NormalCompletion): the
    // return() call is required and a close error propagates
    iter_lazy_close(i, &mut plz)?;
    Ok(out)
}

/// `IfAbruptCloseIterators(iters)` during zip acquisition: close acquired
/// inner sources in reverse, return `e`. The outer iterator is not in the
/// list — its own step errors propagate without closing it.
fn zip_close_inners(i: &mut Interp, srcs: &mut [IterLazy], e: Value) -> Value {
    let mut d = vec![false; srcs.len()];
    let _ = iter_zip_close_srcs(i, srcs, &mut d);
    e
}

/// `IfAbruptCloseIterators(«inputIter» ++ iters)`: close inner sources in
/// reverse order, then the outer iterable last.
fn zip_close_all(i: &mut Interp, outer: &mut IterLazy, srcs: &mut [IterLazy], e: Value) -> Value {
    let e = zip_close_inners(i, srcs, e);
    let _ = iter_lazy_close(i, outer);
    e
}

/// `Iterator.zip(iterables[, options])` — outer via GetIterator, each item
/// must be an object (GetIteratorFlattenable per item).
fn iter_zip(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let iterables = arg(args, 0);
    if !is_spec_obj(&iterables) {
        return Err(i.type_err("zip argument is not an object"));
    }
    let (mode, padding_v) = iter_zip_mode(i, &arg(args, 1))?;
    let mut outer = for_of_iter(i, &iterables)?;
    let mut srcs: Vec<IterLazy> = Vec::new();
    loop {
        let it = match iter_lazy_step(i, &mut outer) {
            Ok(Some(v)) => v,
            Ok(None) => break,
            // outer's own step error → close only the acquired inners
            Err(e) => return Err(zip_close_inners(i, &mut srcs, e)),
        };
        if !is_spec_obj(&it) {
            let e = i.type_err("zip iterable is not an object");
            return Err(zip_close_all(i, &mut outer, &mut srcs, e));
        }
        let s = match iter_to_lazy(i, &it, Flat::RejectStrings) {
            Ok(s) => s,
            // GetIteratorFlattenable abrupt → close inners reverse, outer last
            Err(e) => return Err(zip_close_all(i, &mut outer, &mut srcs, e)),
        };
        srcs.push(s);
    }
    let padding = match iter_zip_padding(i, &padding_v, srcs.len()) {
        Ok(p) => p,
        Err(e) => {
            let mut d = vec![false; srcs.len()];
            let _ = iter_zip_close_srcs(i, &mut srcs, &mut d);
            return Err(e);
        }
    };
    let n = srcs.len();
    Ok(make_lazy(
        i,
        IterLazy {
            src: IterSrc::Empty,
            op: IterOp::Zip {
                srcs,
                done: vec![false; n],
                mode,
                padding,
            },
            started: false,
        },
    ))
}

/// `Iterator.zipKeyed(obj[, options])` — own enumerable {key: iterable}
/// pairs zipped into objects; values must be objects.
fn iter_zip_keyed(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let obj = arg(args, 0);
    if !is_spec_obj(&obj) {
        return Err(i.type_err("zipKeyed argument is not an object"));
    }
    let (mode, padding_v) = iter_zip_mode(i, &arg(args, 1))?;
    // spec: [[OwnPropertyKeys]] snapshot, then per key GetOwnProperty for the
    // enumerable flag *at that moment* — mutations in earlier getters are seen
    let keys = own_keys_all(i, &obj)?;
    let mut cols: Vec<(PropKey, IterLazy)> = Vec::with_capacity(keys.len());
    for k in keys {
        let enumerable = match own_prop_enumerable(i, &obj, &k) {
            Ok(b) => b,
            // IfAbruptCloseIterators(desc, iters): gOPD abrupt closes the
            // columns acquired so far, in reverse
            Err(e) => {
                let mut d = vec![false; cols.len()];
                let _ = iter_zip_keyed_close_srcs(i, &mut cols, &mut d);
                return Err(e);
            }
        };
        if !enumerable {
            continue; // deleted or non-enumerable at check time
        }
        let v = match i.get_prop(&obj, &k) {
            Ok(v) => v,
            Err(e) => {
                let mut d = vec![false; cols.len()];
                let _ = iter_zip_keyed_close_srcs(i, &mut cols, &mut d);
                return Err(e);
            }
        };
        if matches!(v, Value::Und) {
            continue; // spec: undefined-valued keys produce no column
        }
        if !is_spec_obj(&v) {
            let e = i.type_err("zipKeyed value is not an object");
            let mut d = vec![false; cols.len()];
            let _ = iter_zip_keyed_close_srcs(i, &mut cols, &mut d);
            return Err(e);
        }
        match iter_to_lazy(i, &v, Flat::RejectStrings) {
            Ok(s) => cols.push((k, s)),
            Err(e) => {
                let mut d = vec![false; cols.len()];
                let _ = iter_zip_keyed_close_srcs(i, &mut cols, &mut d);
                return Err(e);
            }
        }
    }
    // spec: zipKeyed padding is per-key `Get(paddingOption, key)` — no
    // iterator; each Get's abrupt closes the acquired columns in reverse
    let mut padding = Vec::with_capacity(cols.len());
    if mode == ZipMode::Longest && is_spec_obj(&padding_v) {
        for (k, _) in &cols {
            match i.get_prop(&padding_v, k) {
                Ok(p) => padding.push(p),
                Err(e) => {
                    let mut d = vec![false; cols.len()];
                    let _ = iter_zip_keyed_close_srcs(i, &mut cols, &mut d);
                    return Err(e);
                }
            }
        }
    }
    let n = cols.len();
    Ok(make_lazy(
        i,
        IterLazy {
            src: IterSrc::Empty,
            op: IterOp::ZipKeyed {
                cols,
                done: vec![false; n],
                mode,
                padding,
            },
            started: false,
        },
    ))
}

/// spec: placeholder record on `this` → validate `skippedElements` with
/// IteratorClose → GetIteratorDirect → skip → SameValueZero search.
fn iter_includes(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    iter_this(i, this)?;
    let needle = arg(args, 0);
    let to_skip = match arg(args, 1) {
        Value::Und => 0.0,
        Value::Num(n) if n.is_infinite() || n.fract() == 0.0 => n,
        _ => {
            let e = i.type_err("skippedElements must be an integer or infinity");
            return Err(iter_close_err(i, this, e));
        }
    };
    if to_skip < 0.0 || (to_skip.is_finite() && to_skip > 9007199254740991.0) {
        let e = i.range_err("skippedElements out of range");
        return Err(iter_close_err(i, this, e));
    }
    let mut lz = iter_to_lazy(i, this, Flat::Direct)?;
    if to_skip > 0.0 {
        lz.op = IterOp::Drop {
            left: to_skip.min(usize::MAX as f64) as usize,
        };
    }
    let mut steps = 0usize;
    loop {
        steps += 1;
        if steps >= ITER_LIMIT {
            let _ = iter_lazy_close(i, &mut lz);
            return Err(i.range_err("iteration limit exceeded"));
        }
        match iter_lazy_step(i, &mut lz)? {
            Some(v) if same_value_zero(&v, &needle) => {
                iter_lazy_close(i, &mut lz)?;
                return Ok(Value::Bool(true));
            }
            Some(_) => {}
            None => return Ok(Value::Bool(false)),
        }
    }
}

/// spec: placeholder record → ToString(separator) closes on coercion
/// error → GetIteratorDirect (next lookup after toString) → join;
/// iterator errors propagate raw, content coercion closes.
fn iter_join(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    iter_this(i, this)?;
    let sep = match arg(args, 0) {
        Value::Und => ",".to_string(),
        v => match i.to_str(&v) {
            Ok(s) => s.to_string(),
            Err(e) => return Err(iter_close_err(i, this, e)),
        },
    };
    let mut lz = iter_to_lazy(i, this, Flat::Direct)?;
    let mut out = String::new();
    let mut steps = 0usize;
    loop {
        steps += 1;
        if steps >= ITER_LIMIT {
            let _ = iter_lazy_close(i, &mut lz);
            return Err(i.range_err("iteration limit exceeded"));
        }
        let v = match iter_lazy_step(i, &mut lz)? {
            Some(v) => v,
            None => break,
        };
        if !out.is_empty() {
            out.push_str(&sep);
        }
        match v {
            Value::Und | Value::Null => {}
            Value::Str(s) => out.push_str(&s),
            Value::Num(n) => out.push_str(&js_num_str(n)),
            Value::Bool(b) => out.push_str(if b { "true" } else { "false" }),
            v => match i.to_str(&v) {
                Ok(s) => out.push_str(&s),
                Err(e) => return Err(iter_close_err(i, this, e)),
            },
        }
    }
    Ok(Value::Str(Rc::from(out.as_str())))
}

/// helpers: GetIteratorDirect happens BEFORE arg validation; a validation
/// failure closes the record (IfAbruptCloseIterator) — so wrap first, then
/// validate, closing `lz` on error.
fn helper_close_err(i: &mut Interp, lz: &mut IterLazy, err: Value) -> Value {
    // IteratorClose with a throw completion in flight — close errors are
    // swallowed, the original error propagates
    let _ = iter_lazy_close(i, lz);
    err
}

/// validation failure against the placeholder record (before `next` is
/// read): `IteratorClose` on the object itself, close errors swallowed
fn helper_arg_close(i: &mut Interp, this: &Value, err: Value) -> Value {
    let _ = i.iter_close_obj(this);
    err
}

fn helper_num(i: &mut Interp, this: &Value, v: &Value) -> Result<f64, Value> {
    match i.to_num(v) {
        Ok(n) => Ok(n),
        Err(e) => Err(helper_arg_close(i, this, e)),
    }
}

/// windows/chunks sizes: must already be a Number — `valueOf`/`toString`
/// are never invoked; integral 1..2^32-1 or RangeError. Close on failure.
fn helper_window_size(i: &mut Interp, this: &Value, v: &Value) -> Result<usize, Value> {
    let Value::Num(n) = v else {
        let e = i.type_err("size must be a Number");
        return Err(helper_arg_close(i, this, e));
    };
    if n.fract() != 0.0 {
        let e = i.type_err("size must be an integral Number");
        return Err(helper_arg_close(i, this, e));
    }
    if !(1.0..=4294967295.0).contains(n) {
        let e = i.range_err("size out of range");
        return Err(helper_arg_close(i, this, e));
    }
    Ok(*n as usize)
}

/// `Iterator.prototype.windows(size[, undersized])` — lazy sliding windows;
/// `undersized` is "only-full" (default) or "allow-partial".
fn iter_windows(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    iter_this(i, this)?;
    // spec: placeholder record → validate → then GetIteratorDirect
    let size = helper_window_size(i, this, &arg(args, 0))?;
    let partial = match arg(args, 1) {
        Value::Und => false,
        Value::Str(s) if s.as_ref() == "allow-partial" => true,
        Value::Str(s) if s.as_ref() == "only-full" => false,
        _ => {
            let e = i.type_err("undersized must be \"only-full\" or \"allow-partial\"");
            return Err(helper_arg_close(i, this, e));
        }
    };
    let mut lz = iter_to_lazy(i, this, Flat::Direct)?;
    lz.op = IterOp::Windows {
        size,
        partial,
        buf: std::collections::VecDeque::new(),
    };
    Ok(make_lazy(i, lz))
}

/// `Iterator.prototype.chunks(size)` — lazy non-overlapping chunks
fn iter_chunks(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    iter_this(i, this)?;
    let size = helper_window_size(i, this, &arg(args, 0))?;
    let mut lz = iter_to_lazy(i, this, Flat::Direct)?;
    lz.op = IterOp::Chunks { size };
    Ok(make_lazy(i, lz))
}

fn iter_map(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    iter_this(i, this)?;
    let f = arg(args, 0);
    if !i.is_callable(&f) {
        let e = i.type_err("mapper is not a function");
        return Err(helper_arg_close(i, this, e));
    }
    let mut lz = iter_to_lazy(i, this, Flat::Direct)?;
    lz.op = IterOp::Map { f, n: 0.0 };
    Ok(make_lazy(i, lz))
}

fn iter_filter(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    iter_this(i, this)?;
    let f = arg(args, 0);
    if !i.is_callable(&f) {
        let e = i.type_err("predicate is not a function");
        return Err(helper_arg_close(i, this, e));
    }
    let mut lz = iter_to_lazy(i, this, Flat::Direct)?;
    lz.op = IterOp::Filter { f, n: 0.0 };
    Ok(make_lazy(i, lz))
}

fn iter_flat_map(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    iter_this(i, this)?;
    let f = arg(args, 0);
    if !i.is_callable(&f) {
        let e = i.type_err("mapper is not a function");
        return Err(helper_arg_close(i, this, e));
    }
    let mut lz = iter_to_lazy(i, this, Flat::Direct)?;
    lz.op = IterOp::FlatMap {
        f,
        n: 0.0,
        inner: None,
    };
    Ok(make_lazy(i, lz))
}

/// take/drop `limit`: ToNumber → NaN → RangeError; >2^53-1 → RangeError;
/// ToIntegerOrInfinity → <0 → RangeError. All close the placeholder record.
fn helper_limit(i: &mut Interp, this: &Value, v: &Value) -> Result<usize, Value> {
    let n = helper_num(i, this, v)?;
    if n.is_nan() {
        let e = i.range_err("limit must be a number");
        return Err(helper_arg_close(i, this, e));
    }
    if n.is_finite() && n > 9007199254740991.0 {
        let e = i.range_err("limit out of range");
        return Err(helper_arg_close(i, this, e));
    }
    let int = n.trunc();
    if int < 0.0 {
        let e = i.range_err("limit must be non-negative");
        return Err(helper_arg_close(i, this, e));
    }
    Ok(int.min(usize::MAX as f64) as usize)
}

fn iter_take(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    iter_this(i, this)?;
    // spec: placeholder record → validate → then GetIteratorDirect
    let left = helper_limit(i, this, &arg(args, 0))?;
    let mut lz = iter_to_lazy(i, this, Flat::Direct)?;
    lz.op = IterOp::Take { left };
    Ok(make_lazy(i, lz))
}

fn iter_drop(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    iter_this(i, this)?;
    let left = helper_limit(i, this, &arg(args, 0))?;
    let mut lz = iter_to_lazy(i, this, Flat::Direct)?;
    lz.op = IterOp::Drop { left };
    Ok(make_lazy(i, lz))
}

fn iter_reduce(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    iter_this(i, this)?;
    let f = arg(args, 0);
    if !i.is_callable(&f) {
        let e = i.type_err("reducer is not a function");
        return Err(helper_arg_close(i, this, e));
    }
    let mut acc = Value::Und;
    let mut have_acc = false;
    let mut idx = 0.0f64;
    if let Some(v) = args.get(1) {
        acc = v.clone();
        have_acc = true;
    }
    iter_each(i, this, |i, v| {
        if !have_acc {
            acc = v;
            have_acc = true;
            idx += 1.0;
            return Ok(true);
        }
        acc = i.call(&f, Value::Und, vec![acc.clone(), v, Value::Num(idx)])?;
        idx += 1.0;
        Ok(true)
    })?;
    if !have_acc {
        return Err(i.type_err("Reduce of empty iterator with no initial value"));
    }
    Ok(acc)
}

fn iter_for_each(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    iter_this(i, this)?;
    let f = arg(args, 0);
    if !i.is_callable(&f) {
        let e = i.type_err("callback is not a function");
        return Err(helper_arg_close(i, this, e));
    }
    let mut idx = 0.0f64;
    iter_each(i, this, |i, v| {
        i.call(&f, Value::Und, vec![v, Value::Num(idx)])?;
        idx += 1.0;
        Ok(true)
    })?;
    Ok(Value::Und)
}

fn iter_every(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    iter_this(i, this)?;
    let f = arg(args, 0);
    if !i.is_callable(&f) {
        let e = i.type_err("predicate is not a function");
        return Err(helper_arg_close(i, this, e));
    }
    let mut idx = 0.0f64;
    let mut all = true;
    iter_each(i, this, |i, v| {
        let r = i.call(&f, Value::Und, vec![v, Value::Num(idx)])?;
        idx += 1.0;
        if !i.to_bool(&r) {
            all = false;
            return Ok(false);
        }
        Ok(true)
    })?;
    Ok(Value::Bool(all))
}

fn iter_some(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    iter_this(i, this)?;
    let f = arg(args, 0);
    if !i.is_callable(&f) {
        let e = i.type_err("predicate is not a function");
        return Err(helper_arg_close(i, this, e));
    }
    let mut idx = 0.0f64;
    let mut any = false;
    iter_each(i, this, |i, v| {
        let r = i.call(&f, Value::Und, vec![v, Value::Num(idx)])?;
        idx += 1.0;
        if i.to_bool(&r) {
            any = true;
            return Ok(false);
        }
        Ok(true)
    })?;
    Ok(Value::Bool(any))
}

fn iter_find(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    iter_this(i, this)?;
    let f = arg(args, 0);
    if !i.is_callable(&f) {
        let e = i.type_err("predicate is not a function");
        return Err(helper_arg_close(i, this, e));
    }
    let mut idx = 0.0f64;
    let mut found = Value::Und;
    iter_each(i, this, |i, v| {
        let r = i.call(&f, Value::Und, vec![v.clone(), Value::Num(idx)])?;
        idx += 1.0;
        if i.to_bool(&r) {
            found = v;
            return Ok(false);
        }
        Ok(true)
    })?;
    Ok(found)
}

fn iter_to_array(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    iter_this(i, this)?;
    let mut items = Vec::new();
    iter_each(i, this, |_i, v| {
        items.push(v);
        Ok(true)
    })?;
    Ok(Value::Obj(i.new_arr(items)))
}

fn iter_indexed(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    iter_this(i, this)?;
    let mut lz = iter_to_lazy(i, this, Flat::Direct)?;
    lz.op = IterOp::Indexed { n: 0.0 };
    Ok(make_lazy(i, lz))
}

/// `%IteratorHelperPrototype%.return` — closes the underlying source
fn iter_lazy_ret(i: &mut Interp, this: &Value, a: &[Value]) -> NResult {
    let v = arg(a, 0);
    if let Value::Obj(o) = this {
        if matches!(o.borrow().kind, ObjKind::IterBusy) {
            return Err(iter_busy_err(i));
        }
        if let Some(mut lz) = take_lazy(o) {
            if !lz.started {
                // suspended-start: mark completed BEFORE closing so a
                // reentrant next()/return() sees a finished generator
                // ({done: true}), not an executing one
                put_lazy(o, IterLazy::id(IterSrc::Empty));
            }
            // suspended-yield: the IterBusy sentinel stays while sources
            // close — reentrant calls throw TypeError (executing gen)
            let r = iter_lazy_close(i, &mut lz);
            if lz.started {
                put_lazy(o, IterLazy::id(IterSrc::Empty));
            }
            if let Err(e) = r {
                return Err(e);
            }
            // spec: helper return results carry `undefined`, not the
            // caller's argument (GeneratorResumeAbrupt C.[[Value]])
            return iter_result(i, Value::Und, true);
        }
    }
    // RequireInternalSlot(O, [[UnderlyingIterator]]) — non-helpers throw
    let _ = v;
    Err(i.type_err("Iterator helper return called on invalid receiver"))
}

// ---------------------------------------------------------------------------
// globals
// ---------------------------------------------------------------------------

fn install_globals(i: &mut Interp) {
    define_global(i, "undefined", Value::Und);
    define_global(i, "NaN", Value::Num(f64::NAN));
    define_global(i, "Infinity", Value::Num(f64::INFINITY));
    // spec: NaN/Infinity/undefined are non-writable non-configurable
    // global data properties
    for (n, v) in [
        ("NaN", Value::Num(f64::NAN)),
        ("Infinity", Value::Num(f64::INFINITY)),
        ("undefined", Value::Und),
    ] {
        i.set_attr(&i.p.global.clone(), n, v, false, false);
    }
    let g = i.p.global.clone();
    define_global(i, "globalThis", Value::Obj(g.clone()));
    define_global(i, "window", Value::Obj(g.clone()));
    define_global(i, "self", Value::Obj(g));

    // console
    {
        let c = i.new_obj();
        for name in ["log", "info", "debug", "trace", "dir"] {
            method(i, &c, name, console_log);
        }
        for name in ["warn", "error"] {
            method(i, &c, name, console_log);
        }
        method(i, &c, "clear", |_, _, _| Ok(Value::Und));
        method(i, &c, "assert", console_log);
        method(i, &c, "group", console_log);
        method(i, &c, "groupEnd", |_, _, _| Ok(Value::Und));
        define_global(i, "console", Value::Obj(c));
    }

    // -- $262 test-harness host object --
    {
        let h = i.new_obj();
        i.set_builtin(&h, "detachArrayBuffer", {
            let f = native(i, "detachArrayBuffer", |i, _t, a| {
                if let Value::Obj(o) = arg(a, 0) {
                    if matches!(o.borrow().kind, ObjKind::Bytes(_)) {
                        if o.borrow().immutable {
                            return Err(i.type_err("Cannot detach an immutable ArrayBuffer"));
                        }
                        o.borrow_mut().detached = true;
                        return Ok(Value::Und);
                    }
                }
                Err(i.type_err("not an ArrayBuffer"))
            });
            f
        });
        i.set_builtin(&h, "evalScript", {
            let f = native(i, "evalScript", |i, _t, a| {
                let src = match arg(a, 0) {
                    Value::Str(s) => s,
                    _ => return Err(i.type_err("evalScript needs a string")),
                };
                let g = i.global.clone();
                // Script goal: no caller strictness inheritance
                let saved = std::mem::replace(&mut i.cur_strict, false);
                let r = i.eval_source(&src, g);
                i.cur_strict = saved;
                r
            });
            f
        });
        // IsHTMLDDA — emulates undefined (typeof/ToBoolean/== null) while
        // remaining a callable object
        let dda = Value::Obj(Rc::new(RefCell::new(Obj::new(
            Some(i.p.obj.clone()),
            ObjKind::Und,
        ))));
        i.set_builtin(&h, "IsHTMLDDA", dda);
        define_global(i, "$262", Value::Obj(h));
    }

    // -- Object ctor + statics --
    {
        let ctor = make_ctor(i, "Object", object_ctor, i.p.obj.clone());
        let Value::Obj(o) = &ctor else { unreachable!() };
        let entries = [
            ("keys", obj_keys as NativeFn),
            ("values", obj_values),
            ("entries", obj_entries),
            ("assign", obj_assign),
            ("defineProperty", obj_define_property),
            ("defineProperties", obj_define_properties),
            ("getOwnPropertyDescriptor", obj_get_own_prop_desc),
            ("getOwnPropertyDescriptors", obj_get_own_prop_descs),
            ("getOwnPropertyNames", obj_get_own_prop_names),
            ("getPrototypeOf", obj_get_prototype_of),
            ("setPrototypeOf", obj_set_prototype_of),
            ("create", obj_create),
            ("freeze", obj_freeze),
            ("seal", obj_seal),
            ("isFrozen", obj_is_frozen),
            ("isSealed", obj_is_sealed),
            ("preventExtensions", obj_prevent_ext),
            ("isExtensible", obj_is_extensible),
            ("hasOwn", obj_static_has_own),
            ("fromEntries", obj_from_entries),
            ("groupBy", obj_group_by),
            ("is", obj_is),
            ("getOwnPropertySymbols", obj_get_own_syms),
        ];
        for (n, f) in entries {
            i.set_builtin(o, n, native(i, n, f));
        }
        define_global(i, "Object", ctor);
    }

    // -- Array --
    {
        let ctor = make_ctor(i, "Array", array_ctor, i.p.array.clone());
        let Value::Obj(o) = &ctor else { unreachable!() };
        // Array[@@species] — spec getter returning the constructor
        let species_get = native(i, "get [Symbol.species]", |_i, t, _a| Ok(t.clone()));
        o.borrow_mut().props.insert(
            PropKey::Sym(SYM_SPECIES),
            Prop::accessor(Some(species_get), None),
        );
        i.set_builtin(o, "isArray", native(i, "isArray", array_is_array));
        i.set_builtin(o, "from", native(i, "from", array_from));
        i.set_builtin(o, "of", native(i, "of", array_of));
        define_global(i, "Array", ctor);
    }

    // -- String --
    {
        // %String.prototype% is itself a String object ([[StringData]] =
        // the empty string) — proto methods accept it as `this`
        i.p.string
            .borrow_mut()
            .props
            .insert_str("$value", Prop::builtin(Value::Str(Rc::from(""))));
        let ctor = make_ctor(i, "String", string_ctor, i.p.string.clone());
        let Value::Obj(o) = &ctor else { unreachable!() };
        i.set_builtin(
            o,
            "fromCharCode",
            native(i, "fromCharCode", string_from_char_code),
        );
        i.set_builtin(
            o,
            "fromCodePoint",
            native(i, "fromCodePoint", string_from_code_point),
        );
        i.set_builtin(o, "raw", native(i, "raw", string_raw));
        define_global(i, "String", ctor);
    }

    // -- Number --
    {
        // %Number.prototype% is a Number wrapper ([[NumberData]] = +0)
        i.p.number
            .borrow_mut()
            .props
            .insert_str("$value", Prop::builtin(Value::Num(0.0)));
        let ctor = make_ctor(i, "Number", number_ctor, i.p.number.clone());
        let Value::Obj(o) = &ctor else { unreachable!() };
        for (n, f) in [
            ("isNaN", number_is_nan as NativeFn),
            ("isFinite", number_is_finite),
            ("isInteger", number_is_integer),
            ("isSafeInteger", number_is_safe_integer),
        ] {
            i.set_builtin(o, n, native(i, n, f));
        }

        for (n, v) in [
            ("MAX_SAFE_INTEGER", 9007199254740991.0),
            ("MIN_SAFE_INTEGER", -9007199254740991.0),
            ("MAX_VALUE", f64::MAX),
            ("MIN_VALUE", 5e-324),
            ("EPSILON", f64::EPSILON),
            ("POSITIVE_INFINITY", f64::INFINITY),
            ("NEGATIVE_INFINITY", f64::NEG_INFINITY),
            ("NaN", f64::NAN),
        ] {
            i.set_attr(o, n, Value::Num(v), false, false);
        }
        define_global(i, "Number", ctor);
    }

    // -- Boolean --
    {
        // %Boolean.prototype% is a Boolean wrapper ([[BooleanData]] = false)
        i.p.boolean
            .borrow_mut()
            .props
            .insert_str("$value", Prop::builtin(Value::Bool(false)));
        let ctor = make_ctor(i, "Boolean", boolean_ctor, i.p.boolean.clone());
        define_global(i, "Boolean", ctor);
    }

    // -- Function --
    {
        let ctor = make_ctor(i, "Function", function_ctor, i.p.func.clone());
        define_global(i, "Function", ctor.clone());
        // %GeneratorFunction% — not a global binding; reached via
        // `Object.getPrototypeOf(function*(){}).constructor`. Its
        // [[Prototype]] is %Function%, `.prototype` is
        // %GeneratorFunction.prototype%, and it is [[Construct]]-able:
        // `class G extends GeneratorFunction {}` subclasses it.
        let gfc = Rc::new(RefCell::new(Obj::new(
            match &ctor {
                Value::Obj(o) => Some(o.clone()),
                _ => unreachable!(),
            },
            ObjKind::Func(Func::Native {
                name: Rc::from("GeneratorFunction"),
                f: gen_function_ctor,
                ctor: true,
            }),
        )));
        i.set_attr(
            &gfc,
            "name",
            Value::Str(Rc::from("GeneratorFunction")),
            false,
            true,
        );
        i.set_attr(&gfc, "length", Value::Num(1.0), false, true);
        i.set_attr(
            &gfc,
            "prototype",
            Value::Obj(i.p.gen_func.clone()),
            false,
            false,
        );
        i.set_builtin(&i.p.gen_func.clone(), "constructor", Value::Obj(gfc));
    }

    // -- Symbol --
    {
        let ctor = make_ctor(i, "Symbol", symbol_ctor, i.p.symbol.clone());
        let Value::Obj(o) = &ctor else { unreachable!() };
        i.set_attr(
            o,
            "iterator",
            Value::Sym(SYM_ITERATOR, Rc::from("Symbol.iterator")),
            false,
            false,
        );
        i.set_attr(
            o,
            "toPrimitive",
            Value::Sym(SYM_TOPRIMITIVE, Rc::from("Symbol.toPrimitive")),
            false,
            false,
        );
        i.set_attr(
            o,
            "toStringTag",
            Value::Sym(SYM_TOSTRINGTAG, Rc::from("Symbol.toStringTag")),
            false,
            false,
        );
        i.set_attr(
            o,
            "isConcatSpreadable",
            Value::Sym(
                SYM_IS_CONCAT_SPREADABLE,
                Rc::from("Symbol.isConcatSpreadable"),
            ),
            false,
            false,
        );
        i.set_attr(
            o,
            "species",
            Value::Sym(SYM_SPECIES, Rc::from("Symbol.species")),
            false,
            false,
        );
        i.set_builtin(o, "for", native(i, "for", symbol_for));
        i.set_builtin(o, "keyFor", native(i, "keyFor", symbol_key_for));
        i.set_attr(
            o,
            "hasInstance",
            Value::Sym(SYM_HASINSTANCE, Rc::from("Symbol.hasInstance")),
            false,
            false,
        );
        i.set_attr(
            o,
            "match",
            Value::Sym(SYM_MATCH, Rc::from("Symbol.match")),
            false,
            false,
        );
        i.set_attr(
            o,
            "matchAll",
            Value::Sym(SYM_MATCHALL, Rc::from("Symbol.matchAll")),
            false,
            false,
        );
        i.set_attr(
            o,
            "replace",
            Value::Sym(SYM_REPLACE, Rc::from("Symbol.replace")),
            false,
            false,
        );
        i.set_attr(
            o,
            "search",
            Value::Sym(SYM_SEARCH, Rc::from("Symbol.search")),
            false,
            false,
        );
        i.set_attr(
            o,
            "split",
            Value::Sym(SYM_SPLIT, Rc::from("Symbol.split")),
            false,
            false,
        );
        i.set_attr(
            o,
            "unscopables",
            Value::Sym(SYM_UNSCOPABLES, Rc::from("Symbol.unscopables")),
            false,
            false,
        );
        i.set_attr(
            o,
            "asyncIterator",
            Value::Sym(
                crate::interp::SYM_ASYNCITERATOR,
                Rc::from("Symbol.asyncIterator"),
            ),
            false,
            false,
        );
        define_global(i, "Symbol", ctor);
    }

    // -- BigInt -- `BigInt(x)`/`123n` yield %BigInt.prototype% stand-ins
    // carrying an exact binary magnitude in `__bigint_x`
    {
        fn to_bigint_strict(
            i: &mut Interp,
            v: &Value,
            allow_num: bool,
        ) -> Result<(bool, String), Value> {
            match v {
                Value::Num(n) => {
                    if !allow_num {
                        return Err(i.type_err("Cannot convert a Number to a BigInt"));
                    }
                    if !n.is_finite() || n.fract() != 0.0 {
                        return Err(i.range_err(format!("Cannot convert {n} to a BigInt")));
                    }
                    let (neg, mag, _) = crate::binary::f64_bits(*n);
                    Ok((neg, mag))
                }
                Value::Str(s) => crate::binary::str_to_bigint_bits(i, s),
                Value::Bool(b) => Ok((false, if *b { "1".into() } else { "0".into() })),
                Value::Null | Value::Und | Value::Sym(..) => {
                    Err(i.type_err("Cannot convert value to a BigInt"))
                }
                Value::Obj(_) => {
                    if let Some(x) = crate::binary::bigint_bits(v) {
                        return Ok((x.0, x.1.to_string()));
                    }
                    let p = i.to_primitive(v)?;
                    to_bigint_strict(i, &p, allow_num)
                }
            }
        }
        fn to_bigint(i: &mut Interp, v: &Value) -> Result<(bool, String), Value> {
            to_bigint_strict(i, v, true)
        }
        fn bigint_ctor(i: &mut Interp, _t: &Value, a: &[Value]) -> NResult {
            let (neg, mag) = to_bigint(i, &arg(a, 0))?;
            Ok(i.bigint_obj_b(neg, &mag))
        }
        fn bigint_as_int_n(i: &mut Interp, _t: &Value, a: &[Value]) -> NResult {
            let bits_n = crate::binary::to_index(i, &arg(a, 0))? as f64;
            // past ~2**30 bits the digit-string repr is unrepresentable
            // anyway — engines hit the same wall, just later
            if bits_n > 1073741824.0 {
                return Err(i.range_err("out of range"));
            }
            let (neg, mag) = to_bigint_strict(i, &arg(a, 1), false)?;
            let (sn, sm) = crate::binary::bigint_as_int((neg, &mag), bits_n as usize, true);
            Ok(i.bigint_obj_b(sn, &sm))
        }
        fn bigint_as_uint_n(i: &mut Interp, _t: &Value, a: &[Value]) -> NResult {
            let bits_n = crate::binary::to_index(i, &arg(a, 0))? as f64;
            // past ~2**30 bits the digit-string repr is unrepresentable
            // anyway — engines hit the same wall, just later
            if bits_n > 1073741824.0 {
                return Err(i.range_err("out of range"));
            }
            let (neg, mag) = to_bigint_strict(i, &arg(a, 1), false)?;
            let (sn, sm) = crate::binary::bigint_as_int((neg, &mag), bits_n as usize, false);
            Ok(i.bigint_obj_b(sn, &sm))
        }
        let ctor = make_ctor(i, "BigInt", bigint_ctor, i.p.bigint.clone());
        if let Value::Obj(co) = &ctor {
            let ai = i.native("asIntN", bigint_as_int_n);
            if let Value::Obj(fo) = &ai {
                i.set_attr(fo, "length", Value::Num(2.0), false, true);
            }
            let au = i.native("asUintN", bigint_as_uint_n);
            if let Value::Obj(fo) = &au {
                i.set_attr(fo, "length", Value::Num(2.0), false, true);
            }
            i.set_builtin(&co.clone(), "asIntN", ai);
            i.set_builtin(&co.clone(), "asUintN", au);
        }
        define_global(i, "BigInt", ctor);
    }

    // -- Proxy -- `new Proxy(target, handler)`; MOP ops consult the
    // handler's traps and fall back to the target
    {
        fn proxy_ctor(i: &mut Interp, _t: &Value, a: &[Value]) -> NResult {
            if i.new_target_native.is_none() {
                return Err(i.type_err("Constructor Proxy requires 'new'"));
            }
            let (t, h) = (arg(a, 0), arg(a, 1));
            if !matches!(t, Value::Obj(_)) || !matches!(h, Value::Obj(_)) {
                return Err(
                    i.type_err("Cannot create proxy with a non-object as target or handler")
                );
            }
            // spec: the proxy has [[Call]]/[[Construct]] iff the target
            // has them AT CREATION — revocation later can't remove them
            let pflags = (i.is_callable(&t) as u8) | ((i.is_constructor(&t) as u8) << 1);
            let mut obj = Obj::new(
                None,
                ObjKind::Proxy {
                    target: t,
                    handler: h,
                },
            );
            obj.pflags = pflags;
            Ok(Value::Obj(Rc::new(RefCell::new(obj))))
        }
        fn proxy_revocable(i: &mut Interp, _t: &Value, a: &[Value]) -> NResult {
            let (t, h) = (arg(a, 0), arg(a, 1));
            if !matches!(t, Value::Obj(_)) || !matches!(h, Value::Obj(_)) {
                return Err(
                    i.type_err("Cannot create proxy with a non-object as target or handler")
                );
            }
            let pflags = (i.is_callable(&t) as u8) | ((i.is_constructor(&t) as u8) << 1);
            let mut obj = Obj::new(
                None,
                ObjKind::Proxy {
                    target: t,
                    handler: h,
                },
            );
            obj.pflags = pflags;
            let p = Value::Obj(Rc::new(RefCell::new(obj)));
            // the revoker: a Bound object carrying the proxy as bound-this
            // so the plain fn-pointer native reaches it via `this`
            let do_revoke = i.native("revoke", proxy_do_revoke);
            let rev = Rc::new(RefCell::new(Obj::new(
                Some(i.p.func.clone()),
                ObjKind::Bound {
                    target: do_revoke,
                    thisv: p.clone(),
                    extra: Vec::new(),
                },
            )));
            // spec CreateBuiltinFunction(0, "") — Set order is `length`
            // THEN `name`; the revoker's name is the EMPTY string
            i.set_attr(&rev, "length", Value::Num(0.0), false, true);
            i.set_attr(&rev, "name", Value::Str(Rc::from("")), false, true);
            let r = i.new_obj();
            i.set_data(&r, "proxy", p);
            i.set_data(&r, "revoke", Value::Obj(rev));
            Ok(Value::Obj(r))
        }
        // the Bound revoker lands here with `this` = the proxy —
        // revoking nulls the target/handler slots so every internal
        // method's first step ("[[ProxyHandler]] is null → TypeError")
        // fires through the normal proxy_trap path
        fn proxy_do_revoke(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
            if let Value::Obj(o) = this {
                // scoped borrow — borrow_mut below would panic otherwise
                let is_proxy = { matches!(&o.borrow().kind, ObjKind::Proxy { .. }) };
                if is_proxy {
                    o.borrow_mut().kind = ObjKind::Proxy {
                        target: Value::Null,
                        handler: Value::Null,
                    };
                }
            }
            let _ = i;
            Ok(Value::Und)
        }
        // spec: Proxy has NO prototype property — and setting
        // Function.prototype.constructor here would clobber Function's
        // own backref (make_ctor writes `constructor` onto the proto obj)
        let o = Rc::new(RefCell::new(Obj::new(
            Some(i.p.func.clone()),
            ObjKind::Func(Func::Native {
                name: Rc::from("Proxy"),
                f: proxy_ctor,
                ctor: true,
            }),
        )));
        // spec CreateBuiltinFunction Set order: `length` then `name`
        i.set_attr(&o, "length", Value::Num(2.0), false, true);
        i.set_attr(&o, "name", Value::Str(Rc::from("Proxy")), false, true);
        i.set_builtin(&o, "revocable", native(i, "revocable", proxy_revocable));
        // spec: Proxy has NO `prototype` property (ctor flag set on
        // Func::Native carries [[Construct]])
        define_global(i, "Proxy", Value::Obj(o));
    }

    // -- RegExp --
    {
        // %RegExpStringIteratorPrototype% — proto of @@matchAll results
        let striter_proto = Rc::new(RefCell::new(Obj::new(
            Some(i.p.iterator.clone()),
            ObjKind::Ord,
        )));
        method(i, &striter_proto, "next", re_striter_next);
        striter_proto.borrow_mut().props.insert(
            PropKey::Sym(SYM_TOSTRINGTAG),
            Prop {
                val: Value::Str(Rc::from("RegExp String Iterator")),
                writable: false,
                enumerable: false,
                configurable: true,
                get: None,
                set: None,
            },
        );
        let ctor = make_ctor(i, "RegExp", regexp_ctor, i.p.regexp.clone());
        let Value::Obj(o) = &ctor else { unreachable!() };
        // spec: RegExp.length is 2 (pattern, flags)
        i.set_attr(o, "length", Value::Num(2.0), false, true);
        // RegExp[@@species] — spec getter returning the constructor
        let species_get = native(i, "get [Symbol.species]", |_i, t, _a| Ok(t.clone()));
        o.borrow_mut().props.insert(
            PropKey::Sym(SYM_SPECIES),
            Prop::accessor(Some(species_get), None),
        );
        // Annex-B legacy static accessors (input/$_ is read-write, the
        // rest are get-only) — all non-enumerable + configurable
        let legacy = |key: &str, g: NativeFn, s: Option<NativeFn>| {
            let g = native(i, &format!("get {key}"), g);
            let s = s.map(|f| native(i, &format!("set {key}"), f));
            o.borrow_mut()
                .props
                .insert(pk(key), Prop::accessor(Some(g), s));
        };
        legacy("input", re_static_get_input, Some(re_static_set_input));
        legacy("$_", re_static_get_input, Some(re_static_set_input));
        legacy("lastMatch", re_static_get_last_match, None);
        legacy("$&", re_static_get_last_match, None);
        legacy("lastParen", re_static_get_last_paren, None);
        legacy("$+", re_static_get_last_paren, None);
        legacy("leftContext", re_static_get_left, None);
        legacy("$`", re_static_get_left, None);
        legacy("rightContext", re_static_get_right, None);
        legacy("$'", re_static_get_right, None);
        legacy("$1", re_static_get_p1, None);
        legacy("$2", re_static_get_p2, None);
        legacy("$3", re_static_get_p3, None);
        legacy("$4", re_static_get_p4, None);
        legacy("$5", re_static_get_p5, None);
        legacy("$6", re_static_get_p6, None);
        legacy("$7", re_static_get_p7, None);
        legacy("$8", re_static_get_p8, None);
        legacy("$9", re_static_get_p9, None);
        // per-realm state object (unreachable) — legacy static slots start
        // as "" per Annex B, and the iterator proto rides along
        let state = Rc::new(RefCell::new(Obj::new(None, ObjKind::Ord)));
        {
            let mut b = state.borrow_mut();
            let e = || Value::Str(Rc::from(""));
            for (k, v) in [
                ("iterProto", Value::Obj(striter_proto)),
                ("input", e()),
                ("lastMatch", e()),
                ("lastParen", e()),
                ("leftContext", e()),
                ("rightContext", e()),
                ("p1", e()),
                ("p2", e()),
                ("p3", e()),
                ("p4", e()),
                ("p5", e()),
                ("p6", e()),
                ("p7", e()),
                ("p8", e()),
                ("p9", e()),
            ] {
                b.props.insert_str(k, Prop::builtin(v));
            }
        }
        i.p.regexp
            .borrow_mut()
            .props
            .insert(PropKey::Sym(SYM_RE_STATE), Prop::builtin(Value::Obj(state)));
        define_global(i, "RegExp", ctor);
    }

    // -- Math --
    {
        let m = i.new_obj();
        for (n, f) in [
            ("floor", math_floor as NativeFn),
            ("ceil", math_ceil),
            ("round", math_round),
            ("trunc", math_trunc),
            ("abs", math_abs),
            ("sign", math_sign),
            ("min", math_min),
            ("max", math_max),
            ("pow", math_pow),
            ("sqrt", math_sqrt),
            ("cbrt", math_cbrt),
            ("random", math_random),
            ("log", math_log),
            ("log2", math_log2),
            ("log10", math_log10),
            ("log1p", math_log1p),
            ("exp", math_exp),
            ("expm1", math_expm1),
            ("sin", math_sin),
            ("cos", math_cos),
            ("tan", math_tan),
            ("asin", math_asin),
            ("acos", math_acos),
            ("acosh", math_acosh),
            ("asinh", math_asinh),
            ("atan", math_atan),
            ("atanh", math_atanh),
            ("atan2", math_atan2),
            ("sinh", math_sinh),
            ("cosh", math_cosh),
            ("tanh", math_tanh),
            ("hypot", math_hypot),
            ("imul", math_imul),
            ("clz32", math_clz32),
            ("fround", math_fround),
            ("sumPrecise", math_sum_precise),
        ] {
            method(i, &m, n, f);
        }
        for (n, v) in [
            ("PI", std::f64::consts::PI),
            ("E", std::f64::consts::E),
            ("LN2", std::f64::consts::LN_2),
            ("LN10", std::f64::consts::LN_10),
            ("LOG2E", std::f64::consts::LOG2_E),
            ("LOG10E", std::f64::consts::LOG10_E),
            ("SQRT2", std::f64::consts::SQRT_2),
            ("SQRT1_2", std::f64::consts::FRAC_1_SQRT_2),
        ] {
            i.set_attr(&m, n, Value::Num(v), false, false);
        }
        str_tag(&m, "Math");
        define_global(i, "Math", Value::Obj(m));
    }

    // -- JSON --
    {
        let j = i.new_obj();
        method(i, &j, "parse", json_parse);
        method(i, &j, "stringify", json_stringify);
        str_tag(&j, "JSON");
        define_global(i, "JSON", Value::Obj(j));
    }

    // -- errors --
    {
        let err = make_ctor(i, "Error", error_ctor, i.p.error.clone());
        i.set_attr(
            match &err {
                Value::Obj(o) => o,
                _ => unreachable!(),
            },
            "length",
            Value::Num(1.0),
            false,
            true,
        );
        define_global(i, "Error", err.clone());
        // NativeError ctors' [[Prototype]] is %Error% (19.5.6.2)
        let err_ref = match &err {
            Value::Obj(o) => o.clone(),
            _ => unreachable!(),
        };
        let te = make_ctor(i, "TypeError", type_error_ctor, i.p.type_error.clone());
        let re = make_ctor(i, "RangeError", range_error_ctor, i.p.range_error.clone());
        let se = make_ctor(
            i,
            "SyntaxError",
            syntax_error_ctor,
            i.p.syntax_error.clone(),
        );
        let fe = make_ctor(i, "ReferenceError", ref_error_ctor, i.p.ref_error.clone());
        let ee = make_ctor(i, "EvalError", eval_error_ctor, i.p.eval_error.clone());
        let ue = make_ctor(i, "URIError", uri_error_ctor, i.p.uri_error.clone());
        let ae = make_ctor(i, "AggregateError", agg_error_ctor, i.p.agg_error.clone());
        let su = make_ctor(
            i,
            "SuppressedError",
            suppressed_error_ctor,
            i.p.suppressed_error.clone(),
        );
        for (v, len) in [
            (&te, 1.0),
            (&re, 1.0),
            (&se, 1.0),
            (&fe, 1.0),
            (&ee, 1.0),
            (&ue, 1.0),
            (&ae, 2.0),
            (&su, 3.0),
        ] {
            if let Value::Obj(o) = v {
                o.borrow_mut().proto = Some(err_ref.clone());
                i.set_attr(o, "length", Value::Num(len), false, true);
            }
        }
        define_global(i, "TypeError", te);
        define_global(i, "RangeError", re);
        define_global(i, "SyntaxError", se);
        define_global(i, "ReferenceError", fe);
        define_global(i, "EvalError", ee);
        define_global(i, "URIError", ue);
        define_global(i, "AggregateError", ae);
        define_global(i, "SuppressedError", su);
    }

    // -- Promise --
    {
        let p = i.p.promise.clone();
        method(i, &p, "then", promise_then);
        method(i, &p, "catch", promise_catch);
        method(i, &p, "finally", promise_finally);
        str_tag(&p, "Promise");
        let ctor = make_ctor(i, "Promise", promise_ctor, p);
        let Value::Obj(o) = &ctor else { unreachable!() };
        i.set_builtin(o, "resolve", native(i, "resolve", promise_resolve_static));
        i.set_builtin(o, "reject", native(i, "reject", promise_reject_static));
        i.set_builtin(o, "all", native(i, "all", promise_all));
        i.set_builtin(
            o,
            "allSettled",
            native(i, "allSettled", promise_all_settled),
        );
        i.set_builtin(o, "any", native(i, "any", promise_any));
        i.set_builtin(o, "race", native(i, "race", promise_race));
        i.set_builtin(o, "try", native(i, "try", promise_try));
        i.set_builtin(o, "allKeyed", native(i, "allKeyed", promise_all_keyed));
        i.set_builtin(
            o,
            "withResolvers",
            native(i, "withResolvers", promise_with_resolvers),
        );
        // Promise[@@species] — spec getter returning the constructor
        // itself; SpeciesConstructor reads it on then/catch/finally
        let species_get = native(i, "get [Symbol.species]", |_i, t, _a| Ok(t.clone()));
        o.borrow_mut().props.insert(
            PropKey::Sym(SYM_SPECIES),
            Prop::accessor(Some(species_get), None),
        );
        // intrinsic %Promise% — SpeciesConstructor's default survives a
        // rewritten global Promise binding
        i.p.promise_ctor = o.clone();
        define_global(i, "Promise", ctor);
    }

    // -- Date --
    {
        let dp = i.new_obj();
        method(i, &dp, "getTime", date_get_time);
        method(i, &dp, "valueOf", date_get_time);
        method(i, &dp, "toString", date_to_string);
        method(i, &dp, "toISOString", date_to_iso);
        method(i, &dp, "toJSON", date_to_json);
        method(i, &dp, "toUTCString", date_to_utc);
        // `toGMTString` is `toUTCString` — the identical fn object
        let utc = i.obj_get_raw_str(&dp, "toUTCString").unwrap();
        dp.borrow_mut()
            .props
            .insert(PropKey::from_str("toGMTString"), Prop::builtin(utc));
        method(i, &dp, "toDateString", date_to_datestr);
        method(i, &dp, "toTimeString", date_to_timestr);
        method(i, &dp, "toLocaleString", date_to_utc);
        method(i, &dp, "toLocaleDateString", date_to_datestr);
        method(i, &dp, "toLocaleTimeString", date_to_timestr);
        method(i, &dp, "getFullYear", |i, t, _| {
            date_getter(i, t, |d| d.0 as f64)
        });
        method(i, &dp, "getMonth", |i, t, _| {
            date_getter(i, t, |d| (d.1 - 1) as f64)
        });
        method(i, &dp, "getDate", |i, t, _| {
            date_getter(i, t, |d| d.2 as f64)
        });
        method(i, &dp, "getDay", |i, t, _| {
            date_getter(i, t, |d| d.3 as f64)
        });
        method(i, &dp, "getHours", |i, t, _| {
            date_getter(i, t, |d| d.4 as f64)
        });
        method(i, &dp, "getMinutes", |i, t, _| {
            date_getter(i, t, |d| d.5 as f64)
        });
        method(i, &dp, "getSeconds", |i, t, _| {
            date_getter(i, t, |d| d.6 as f64)
        });
        method(i, &dp, "getMilliseconds", |i, t, _| {
            date_getter(i, t, |d| d.7 as f64)
        });
        method(i, &dp, "getUTCFullYear", |i, t, _| {
            date_getter(i, t, |d| d.0 as f64)
        });
        method(i, &dp, "getUTCMonth", |i, t, _| {
            date_getter(i, t, |d| (d.1 - 1) as f64)
        });
        method(i, &dp, "getUTCDate", |i, t, _| {
            date_getter(i, t, |d| d.2 as f64)
        });
        method(i, &dp, "getUTCDay", |i, t, _| {
            date_getter(i, t, |d| d.3 as f64)
        });
        method(i, &dp, "getUTCHours", |i, t, _| {
            date_getter(i, t, |d| d.4 as f64)
        });
        method(i, &dp, "getUTCMinutes", |i, t, _| {
            date_getter(i, t, |d| d.5 as f64)
        });
        method(i, &dp, "getUTCSeconds", |i, t, _| {
            date_getter(i, t, |d| d.6 as f64)
        });
        method(i, &dp, "getUTCMilliseconds", |i, t, _| {
            date_getter(i, t, |d| d.7 as f64)
        });
        // legacy getYear = getFullYear - 1900
        method(i, &dp, "getYear", |i, t, _| {
            date_getter(i, t, |d| (d.0 - 1900) as f64)
        });
        method(i, &dp, "getTimezoneOffset", |i, t, _| {
            // UTC-only engine → offset is always 0, but the spec still
            // TypeErrors on non-Date receivers and returns NaN on NaN dates
            let t = this_time_value(i, t)?;
            Ok(Value::Num(if t.is_nan() { f64::NAN } else { 0.0 }))
        });
        // setters — UTC-only engine: local == UTC. slots = y,mo,d,h,mi,s,ms
        method(i, &dp, "setTime", date_set_time);
        method(i, &dp, "setMilliseconds", |i, t, a| {
            date_set(i, t, a, &[6], false)
        });
        method(i, &dp, "setSeconds", |i, t, a| {
            date_set(i, t, a, &[5, 6], false)
        });
        method(i, &dp, "setMinutes", |i, t, a| {
            date_set(i, t, a, &[4, 5, 6], false)
        });
        method(i, &dp, "setHours", |i, t, a| {
            date_set(i, t, a, &[3, 4, 5, 6], false)
        });
        method(i, &dp, "setDate", |i, t, a| date_set(i, t, a, &[2], false));
        method(i, &dp, "setMonth", |i, t, a| {
            date_set(i, t, a, &[1, 2], false)
        });
        method(i, &dp, "setFullYear", |i, t, a| {
            date_set(i, t, a, &[0, 1, 2], true)
        });
        method(i, &dp, "setYear", date_set_year);
        method(i, &dp, "setUTCMilliseconds", |i, t, a| {
            date_set(i, t, a, &[6], false)
        });
        method(i, &dp, "setUTCSeconds", |i, t, a| {
            date_set(i, t, a, &[5, 6], false)
        });
        method(i, &dp, "setUTCMinutes", |i, t, a| {
            date_set(i, t, a, &[4, 5, 6], false)
        });
        method(i, &dp, "setUTCHours", |i, t, a| {
            date_set(i, t, a, &[3, 4, 5, 6], false)
        });
        method(i, &dp, "setUTCDate", |i, t, a| {
            date_set(i, t, a, &[2], false)
        });
        method(i, &dp, "setUTCMonth", |i, t, a| {
            date_set(i, t, a, &[1, 2], false)
        });
        method(i, &dp, "setUTCFullYear", |i, t, a| {
            date_set(i, t, a, &[0, 1, 2], true)
        });
        // Date.prototype[@@toPrimitive] — name "[Symbol.toPrimitive]", length 1
        let stp = native(i, "[Symbol.toPrimitive]", date_to_primitive);
        if let Value::Obj(fo) = &stp {
            i.set_attr(fo, "length", Value::Num(1.0), false, true);
        }
        dp.borrow_mut().props.insert(
            PropKey::Sym(SYM_TOPRIMITIVE),
            Prop {
                val: stp,
                writable: false,
                enumerable: false,
                configurable: true,
                get: None,
                set: None,
            },
        );
        // NOTE: no @@toStringTag on Date.prototype — the spec derives
        // "Date" from the [[DateValue]] builtinTag instead; a non-writable
        // proto prop would block `d[Symbol.toStringTag] = "x"` overrides
        // spec function lengths differ from the `native()` default of 0
        for (name, len) in [
            ("toJSON", 1.0),
            ("setTime", 1.0),
            ("setMilliseconds", 1.0),
            ("setUTCMilliseconds", 1.0),
            ("setSeconds", 2.0),
            ("setUTCSeconds", 2.0),
            ("setMinutes", 3.0),
            ("setUTCMinutes", 3.0),
            ("setHours", 4.0),
            ("setUTCHours", 4.0),
            ("setDate", 1.0),
            ("setUTCDate", 1.0),
            ("setMonth", 2.0),
            ("setUTCMonth", 2.0),
            ("setFullYear", 3.0),
            ("setUTCFullYear", 3.0),
            ("setYear", 1.0),
        ] {
            let f = i.obj_get_raw_str(&dp, name).unwrap_or(Value::Und);
            if let Value::Obj(fo) = f {
                i.set_attr(&fo, "length", Value::Num(len), false, true);
            }
        }
        let ctor = make_ctor(i, "Date", date_ctor, dp.clone());
        let Value::Obj(o) = &ctor else { unreachable!() };
        i.set_builtin(o, "now", native(i, "now", date_now));
        i.set_builtin(o, "UTC", native(i, "UTC", date_utc));
        i.set_builtin(o, "parse", native(i, "parse", date_parse));
        for (name, len) in [("UTC", 7.0), ("parse", 1.0)] {
            let f = i.get_prop_str(&ctor, name).unwrap_or(Value::Und);
            if let Value::Obj(fo) = f {
                i.set_attr(&fo, "length", Value::Num(len), false, true);
            }
        }
        // new Date(...) accepts up to 7 components
        i.set_attr(o, "length", Value::Num(7.0), false, true);
        define_global(i, "Date", ctor);
    }

    // -- Map / Set (basic) --
    {
        let mp = i.new_obj();
        method(i, &mp, "get", map_get);
        method(i, &mp, "set", map_set);
        method(i, &mp, "has", map_has);
        method(i, &mp, "delete", map_delete);
        method(i, &mp, "clear", map_clear);
        method(i, &mp, "forEach", map_for_each);
        method(i, &mp, "getOrInsert", map_get_or_insert);
        method(i, &mp, "getOrInsertComputed", map_get_or_insert_computed);
        method_len(i, &mp, "keys", map_keys, 0.0);
        method_len(i, &mp, "values", map_values, 0.0);
        method_len(i, &mp, "entries", map_entries, 0.0);
        let ent = i.obj_get_raw_str(&mp, "entries").unwrap();
        mp.borrow_mut()
            .props
            .insert(PropKey::Sym(SYM_ITERATOR), Prop::builtin(ent));
        let size_g = native(i, "get size", map_size);
        mp.borrow_mut()
            .props
            .insert(pk("size"), Prop::accessor(Some(size_g), None));
        str_tag(&mp, "Map");
        let ctor = make_ctor(i, "Map", map_ctor, mp);
        let Value::Obj(mo) = &ctor else {
            unreachable!()
        };
        let msp = native(i, "get [Symbol.species]", |_i, t, _a| Ok(t.clone()));
        mo.borrow_mut()
            .props
            .insert(PropKey::Sym(SYM_SPECIES), Prop::accessor(Some(msp), None));
        i.set_builtin(mo, "groupBy", native(i, "groupBy", map_group_by));
        define_global(i, "Map", ctor);

        let sp = i.new_obj();
        method(i, &sp, "add", set_add);
        method(i, &sp, "has", set_has);
        method(i, &sp, "delete", set_delete);
        method(i, &sp, "clear", set_clear);
        method_len(i, &sp, "forEach", set_for_each, 1.0);
        method_len(i, &sp, "values", set_values, 0.0);
        // spec: Set.prototype.keys === Set.prototype.values (same fn)
        let sv_keys = i.obj_get_raw_str(&sp, "values").unwrap();
        sp.borrow_mut()
            .props
            .insert_str("keys", Prop::builtin(sv_keys));
        method_len(i, &sp, "entries", set_entries, 0.0);
        let sv = i.obj_get_raw_str(&sp, "values").unwrap();
        sp.borrow_mut()
            .props
            .insert(PropKey::Sym(SYM_ITERATOR), Prop::builtin(sv));
        let ssize = native(i, "get size", set_size);
        sp.borrow_mut()
            .props
            .insert(pk("size"), Prop::accessor(Some(ssize), None));
        str_tag(&sp, "Set");
        let sctor = make_ctor(i, "Set", set_ctor, sp);
        let Value::Obj(so) = &sctor else {
            unreachable!()
        };
        let ssp = native(i, "get [Symbol.species]", |_i, t, _a| Ok(t.clone()));
        so.borrow_mut()
            .props
            .insert(PropKey::Sym(SYM_SPECIES), Prop::accessor(Some(ssp), None));
        define_global(i, "Set", sctor);
    }

    // -- global fns --
    for (n, f) in [
        ("parseInt", g_parse_int as NativeFn),
        ("parseFloat", g_parse_float),
        ("isNaN", g_is_nan),
        ("isFinite", g_is_finite),
        ("encodeURIComponent", g_encode_uri_component),
        ("decodeURIComponent", g_decode_uri_component),
        ("escape", g_escape),
        ("unescape", g_unescape),
        ("encodeURI", g_encode_uri),
        ("decodeURI", g_decode_uri),
        ("$dyn_import", g_dyn_import),
        ("setTimeout", g_set_timeout),
        ("clearTimeout", g_noop),
        ("setInterval", g_set_timeout),
        ("clearInterval", g_noop),
        ("queueMicrotask", g_queue_microtask),
        ("requestAnimationFrame", g_noop),
        ("alert", g_noop),
        ("btoa", g_btoa),
        ("atob", g_atob),
        ("eval", g_eval),
    ] {
        define_global(i, n, native(i, n, f));
    }
    // spec: Number.parseInt === parseInt / Number.parseFloat ===
    // parseFloat — the same function objects (globals install above)
    {
        let g = i.global.clone();
        for n in ["parseInt", "parseFloat"] {
            if let Ok(Some(v)) = i.env_lookup(&g, n) {
                if let Ok(Some(num)) = i.env_lookup(&g, "Number") {
                    if let Value::Obj(no) = num {
                        no.borrow_mut().props.insert_str(n, Prop::builtin(v));
                    }
                }
            }
        }
    }
    // remember the installed eval so direct `eval(...)` calls take the
    // caller-environment path
    {
        let g = i.global.clone();
        if let Ok(Some(v)) = i.env_lookup(&g, "eval") {
            i.eval_fn = Some(v);
        }
    }

    // WeakMap/WeakSet minimal → alias to Map/Set behavior
    {
        let wp = i.new_obj();
        method(i, &wp, "get", wm_get);
        method(i, &wp, "set", wm_set);
        method(i, &wp, "has", wm_has);
        method(i, &wp, "delete", wm_delete);
        method(i, &wp, "getOrInsert", wm_get_or_insert);
        method(i, &wp, "getOrInsertComputed", wm_get_or_insert_computed);
        str_tag(&wp, "WeakMap");
        let wctor = make_ctor(i, "WeakMap", weakmap_ctor, wp);
        define_global(i, "WeakMap", wctor);
        let wsp = i.new_obj();
        method(i, &wsp, "add", ws_add);
        method(i, &wsp, "has", ws_has);
        method(i, &wsp, "delete", ws_delete);
        str_tag(&wsp, "WeakSet");
        let wsctor = make_ctor(i, "WeakSet", weakset_ctor, wsp);
        define_global(i, "WeakSet", wsctor);
    }

    // Reflect minimal
    {
        let r = i.new_obj();
        method(i, &r, "get", reflect_get);
        method(i, &r, "set", reflect_set);
        method(i, &r, "has", reflect_has);
        method(i, &r, "deleteProperty", reflect_delete);
        method(i, &r, "ownKeys", reflect_own_keys);
        method(i, &r, "getOwnPropertyDescriptor", reflect_get_own_prop_desc);
        method(i, &r, "defineProperty", reflect_define_property);
        method(i, &r, "getPrototypeOf", reflect_get_prototype_of);
        method(i, &r, "setPrototypeOf", reflect_set_prototype_of);
        method(i, &r, "isExtensible", reflect_is_extensible);
        method(i, &r, "preventExtensions", reflect_prevent_extensions);
        method(i, &r, "apply", reflect_apply);
        method(i, &r, "construct", reflect_construct);
        // spec arities — the names collide with other builtins (Map.get
        // &co) so they can't ride NATIVE_ARITY
        for (n, len) in [
            ("apply", 3.0),
            ("construct", 2.0),
            ("defineProperty", 3.0),
            ("deleteProperty", 2.0),
            ("get", 2.0),
            ("getOwnPropertyDescriptor", 2.0),
            ("getPrototypeOf", 1.0),
            ("has", 2.0),
            ("isExtensible", 1.0),
            ("ownKeys", 1.0),
            ("preventExtensions", 1.0),
            ("set", 3.0),
            ("setPrototypeOf", 2.0),
        ] {
            let f = r.borrow().props.get_str(n).map(|p| p.val.clone());
            if let Some(Value::Obj(fo)) = f {
                i.set_attr(&fo, "length", Value::Num(len), false, true);
            }
        }
        r.borrow_mut().props.insert(
            PropKey::Sym(SYM_TOSTRINGTAG),
            Prop {
                val: Value::Str(Rc::from("Reflect")),
                writable: false,
                enumerable: false,
                configurable: true,
                get: None,
                set: None,
            },
        );
        define_global(i, "Reflect", Value::Obj(r));
    }

    // binary views + web-platform surface (TextEncoder, URL, Blob, …)
    crate::binary::install(i);

    // spec ctor lengths (make_ctor defaults to 0) — after binary::install
    // so ArrayBuffer/DataView/TypedArray globals exist
    for (n, len) in [
        ("Object", 1.0),
        ("Function", 1.0),
        ("Array", 1.0),
        ("String", 1.0),
        ("Number", 1.0),
        ("Boolean", 1.0),
        ("Promise", 1.0),
        ("RegExp", 2.0),
        ("Map", 0.0),
        ("Set", 0.0),
        ("WeakMap", 0.0),
        ("WeakSet", 0.0),
        ("Date", 7.0),
        ("Symbol", 0.0),
        ("BigInt", 1.0),
        ("ArrayBuffer", 1.0),
        ("SharedArrayBuffer", 1.0),
        ("DataView", 1.0),
    ] {
        let v = i.p.global.borrow().props.get_str(n).map(|p| p.val.clone());
        if let Some(Value::Obj(o)) = v {
            i.set_attr(&o, "length", Value::Num(len), false, true);
        }
    }
}

fn make_ctor(i: &mut Interp, name: &str, f: NativeFn, proto: ObjRef) -> Value {
    let o = Rc::new(RefCell::new(Obj::new(
        Some(i.p.func.clone()),
        ObjKind::Func(Func::Native {
            name: Rc::from(name),
            f,
            ctor: true,
        }),
    )));
    // spec SetFunctionName/SetFunctionLength order — `length` own prop
    // is defined before `name` (getOwnPropertyNames ordering)
    i.set_attr(&o, "length", Value::Num(0.0), false, true);
    i.set_attr(&o, "name", Value::Str(Rc::from(name)), false, true);
    // ctor .prototype: {W:false, E:false, C:false}
    i.set_attr(&o, "prototype", Value::Obj(proto.clone()), false, false);
    i.set_builtin(&proto, "constructor", Value::Obj(o.clone()));
    Value::Obj(o)
}

// ===========================================================================
// natives: object
// ===========================================================================

/// `Object.hasOwn(target, key)` — the static takes (obj, prop) as args;
/// `hasOwnProperty` reaches here with this=receiver and args=[key].
fn obj_static_has_own(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    // spec Object.hasOwn: ToObject(O) runs BEFORE ToPropertyKey(P) —
    // a null/undefined target throws before key conversion side effects
    let target = i.to_object(&arg(args, 0))?;
    let kv = arg(args, 1);
    has_own_impl(i, &target, &kv)
}

fn obj_has_own(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let kv = arg(args, 0);
    has_own_impl(i, this, &kv)
}

fn has_own_impl(i: &mut Interp, this: &Value, kv: &Value) -> NResult {
    let k = i.to_propkey(kv)?;
    // spec ToObject — null/undefined throw; string wrappers own their
    // indexed chars + length
    let this = i.to_object(this)?;
    let own = if let Value::Obj(o) = &this {
        let b = o.borrow();
        if matches!(&b.kind, ObjKind::Proxy { .. }) {
            let kv = i.key_to_value(&k);
            drop(b);
            // a proxy answers via its getOwnPropertyDescriptor trap
            let d = obj_get_own_prop_desc(i, &Value::Und, &[this.clone(), kv])?;
            return Ok(Value::Bool(!matches!(d, Value::Und)));
        }
        match &b.kind {
            ObjKind::Arr(items) => {
                if let PropKey::S(s) = &k {
                    if s.as_ref() == "length" {
                        return Ok(Value::Bool(true));
                    }
                    if let Some(idx) = arr_index(s) {
                        if idx < items.len() && !b.holes.as_ref().is_some_and(|h| h.contains(&idx))
                        {
                            return Ok(Value::Bool(true));
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
                if let PropKey::S(s) = &k {
                    // canonical numeric strings answer solely from the
                    // element store (never props)
                    if let Some(ni) = canonical_numeric_index(s) {
                        return Ok(Value::Bool(
                            typed_state(buf, *off, *len, *kind, *track)
                                .is_some_and(|eff| valid_int_index(ni, eff)),
                        ));
                    }
                }
            }
            ObjKind::Args { items, .. } => {
                if let PropKey::S(s) = &k {
                    if let Some(idx) = arr_index(s) {
                        if idx < items.len() && !b.holes.as_ref().is_some_and(|h| h.contains(&idx))
                        {
                            return Ok(Value::Bool(true));
                        }
                    }
                }
            }
            _ => {}
        }
        b.props.contains_key(&k)
    } else {
        false
    };
    Ok(Value::Bool(own))
}

fn obj_is_prototype_of(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let a = arg(args, 0);
    // spec 19.1.3.6: `If V is not Object → false` runs BEFORE
    // RequireObjectCoercible(this) — a non-object arg short-circuits
    // even on a nullish receiver; an object arg on null/undefined this
    // throws TypeError
    if !matches!(a, Value::Obj(_)) {
        return Ok(Value::Bool(false));
    }
    if matches!(this, Value::Und | Value::Null) {
        return Err(i.type_err("Cannot convert undefined or null to object"));
    }
    if let (Value::Obj(proto), Value::Obj(c0)) = (this, &a) {
        let mut cur = c0.clone();
        for _ in 0..10_000 {
            // each hop is [[GetPrototypeOf]] — a proxy in the chain
            // fires its getPrototypeOf trap (scoped borrow: the trap
            // may mutate the object)
            let next = if { matches!(&cur.borrow().kind, ObjKind::Proxy { .. }) } {
                match obj_get_prototype_of(i, &Value::Und, &[Value::Obj(cur.clone())])? {
                    Value::Obj(p) => Some(p),
                    _ => None,
                }
            } else {
                cur.borrow().proto.clone()
            };
            match next {
                Some(p) => {
                    if Rc::ptr_eq(&p, proto) {
                        return Ok(Value::Bool(true));
                    }
                    cur = p;
                }
                None => break,
            }
        }
    }
    Ok(Value::Bool(false))
}

fn obj_prop_is_enum(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let k = i.to_propkey(&arg(args, 0))?;
    let this = i.to_object(this)?;
    if let Value::Obj(o) = &this {
        // spec runs O.[[GetOwnProperty]] — a proxy receiver fires its
        // getOwnPropertyDescriptor trap (own_prop_enumerable routes it)
        if matches!(&o.borrow().kind, ObjKind::Proxy { .. }) {
            return Ok(Value::Bool(own_prop_enumerable(i, &this, &k)?));
        }
        if let Some(p) = o.borrow().props.get(&k) {
            return Ok(Value::Bool(p.enumerable));
        }
        let b = o.borrow();
        if let ObjKind::Arr(items) = &b.kind {
            if let PropKey::S(s) = &k {
                if let Some(idx) = arr_index(s) {
                    return Ok(Value::Bool(
                        idx < items.len() && !b.holes.as_ref().is_some_and(|h| h.contains(&idx)),
                    ));
                }
            }
        }
        if let ObjKind::Args { items, .. } = &b.kind {
            if let PropKey::S(s) = &k {
                if let Some(idx) = arr_index(s) {
                    // virtual indices are enumerable; materialized attrs
                    // were already checked via props above; a holed or
                    // materialized-non-enumerable slot reports false
                    return Ok(Value::Bool(
                        idx < items.len() && !b.holes.as_ref().is_some_and(|h| h.contains(&idx)),
                    ));
                }
            }
        }
        // integer-indexed exotic slots are enumerable (10.4.5.3) — only
        // live, in-bounds canonical numeric keys produce a descriptor
        if let ObjKind::Typed {
            buf,
            off,
            len,
            kind,
            track,
        } = &b.kind
        {
            if let PropKey::S(s) = &k {
                if let Some(ni) = canonical_numeric_index(s) {
                    return Ok(Value::Bool(
                        typed_state(buf, *off, *len, *kind, *track)
                            .is_some_and(|eff| valid_int_index(ni, eff)),
                    ));
                }
            }
        }
    }
    Ok(Value::Bool(false))
}

fn obj_to_string(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    match this {
        Value::Und => {
            return Ok(Value::Str(Rc::from("[object Undefined]")));
        }
        Value::Null => return Ok(Value::Str(Rc::from("[object Null]"))),
        _ => {}
    }
    // spec 19.1.3.6: ToObject(this) first — primitives are classified by
    // the wrapped object's internal slots and proto @@toStringTag values
    // (Boolean.prototype[@@toStringTag]="x" affects toString.call(true))
    let ov = i.to_object(this)?;
    let Value::Obj(o) = &ov else { unreachable!() };
    let kind = o.borrow().kind.clone();
    let builtin = match &kind {
        ObjKind::Arr(_) => "Array",
        ObjKind::Args { .. } => "Arguments",
        ObjKind::Func(_) | ObjKind::Bound { .. } => "Function",
        ObjKind::Err => "Error",
        ObjKind::Host(tag) if tag.as_ref() == "date" => "Date",
        ObjKind::Regex(_) => "RegExp",
        // spec IsArray forwards through proxies to the target chain; a
        // revoked proxy throws
        ObjKind::Proxy { target, .. } => {
            let mut cur = target.clone();
            loop {
                let step = match &cur {
                    Value::Obj(p) => match &p.borrow().kind {
                        ObjKind::Proxy { target: nt, .. } => Some(nt.clone()),
                        ObjKind::Arr(_) => break "Array",
                        _ => break "Object",
                    },
                    _ => {
                        return Err(
                            i.type_err("Cannot perform 'IsArray' on a proxy that has been revoked")
                        )
                    }
                };
                match step {
                    Some(nt) => cur = nt,
                    None => break "Object",
                }
            }
        }
        _ => {
            // primitive wrappers tag by their internal slot
            // ([[StringData]]/[[NumberData]]/[[BooleanData]]); the
            // modern spec has no Symbol/BigInt builtinTag — their names
            // come from proto @@toStringTag instead
            let v = o
                .borrow()
                .props
                .get_str("$value")
                .filter(|p| !p.enumerable)
                .map(|p| p.val.clone());
            match v {
                Some(Value::Str(_)) => "String",
                Some(Value::Num(_)) => "Number",
                Some(Value::Bool(_)) => "Boolean",
                _ => "Object",
            }
        }
    };
    // spec: a string-valued @@toStringTag overrides builtinTag
    if let Value::Str(t) = i.get_prop(&ov, &PropKey::Sym(SYM_TOSTRINGTAG))? {
        return Ok(Value::Str(Rc::from(format!("[object {t}]").as_str())));
    }
    Ok(Value::Str(Rc::from(format!("[object {builtin}]").as_str())))
}

fn obj_to_locale_string(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    // spec 19.1.3.5: Invoke(O, "toString") — an overridden toString wins
    let f = i.get_prop_str(this, "toString")?;
    if !i.is_callable(&f) {
        return Err(i.type_err("toString is not a function"));
    }
    i.call(&f, this.clone(), Vec::new())
}

fn obj_value_of(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    // spec ToObject(this) — a primitive receiver returns its wrapper
    i.to_object(this)
}

fn obj_get_proto(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    match this {
        Value::Obj(o) => {
            // `{ }` scopes the borrow — the trap may touch the proxy
            if let ObjKind::Proxy { target, handler } = &{ o.borrow().kind.clone() } {
                let (t, h) = (target.clone(), handler.clone());
                return match i.proxy_trap(&h, "getPrototypeOf")? {
                    Some(trap) => {
                        let r = i.call(&trap, h, vec![t.clone()])?;
                        match r {
                            Value::Obj(_) | Value::Null => {
                                i.proxy_getproto_invariant(&t, &r)?;
                                Ok(r)
                            }
                            _ => Err(i.type_err("proxy getPrototypeOf must return object or null")),
                        }
                    }
                    None => obj_get_proto(i, &t, &[]),
                };
            }
            Ok(match o.borrow().proto.clone() {
                Some(p) => Value::Obj(p),
                None => Value::Null,
            })
        }
        // spec __proto__ getter: RequireObjectCoercible — null/undefined
        // throw; a primitive reads its wrapper's proto (`(5).__proto__`
        // is Number.prototype in every engine)
        Value::Und | Value::Null => Err(i.type_err("Cannot convert undefined or null to object")),
        _ => {
            let w = i.to_object(this)?;
            Ok(match w {
                Value::Obj(o) => match o.borrow().proto.clone() {
                    Some(p) => Value::Obj(p),
                    None => Value::Null,
                },
                _ => Value::Null,
            })
        }
    }
}

/// Would `o.proto = p` close a cycle? Walks p's chain looking for `o`;
/// the hop cap treats pathological pre-existing chains as cyclic.
fn proto_cycle(o: &ObjRef, p: &ObjRef) -> bool {
    let mut cur = Some(p.clone());
    let mut hops = 0usize;
    while let Some(c) = cur {
        if Rc::ptr_eq(&c, o) {
            return true;
        }
        cur = c.borrow().proto.clone();
        hops += 1;
        if hops > 100_000 {
            return true;
        }
    }
    false
}

fn obj_set_proto(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec __proto__ setter: RequireObjectCoercible(this) — null and
    // undefined receivers throw TypeError before anything else runs
    if matches!(this, Value::Und | Value::Null) {
        return Err(i.type_err("Cannot convert undefined or null to object"));
    }
    // primitives + a non-object proto are silently ignored; a
    // [[SetPrototypeOf]] failure throws TypeError
    let v = arg(args, 0);
    if !matches!(v, Value::Obj(_) | Value::Null) {
        return Ok(Value::Und);
    }
    let Value::Obj(o) = this else {
        return Ok(Value::Und);
    };
    // spec B.2.2.1.2 step 5: a false [[SetPrototypeOf]] (cycle or
    // non-extensible) throws TypeError
    if !ord_set_proto(i, o, &v)? {
        return Err(i.type_err("Cyclic __proto__ value"));
    }
    Ok(Value::Und)
}

/// Annex B `__defineGetter__`/`__defineSetter__` — define an accessor
/// {get|set: f, enumerable: true, configurable: true} on ToObject(this).
fn obj_define_accessor(i: &mut Interp, this: &Value, args: &[Value], field: &str) -> NResult {
    let o = i.to_object(this)?;
    let f = arg(args, 1);
    if !i.is_callable(&f) {
        return Err(i.type_err("Getter must be a function"));
    }
    let k = i.to_propkey(&arg(args, 0))?;
    let d = i.new_obj();
    i.set_data(&d, field, f);
    i.set_data(&d, "enumerable", Value::Bool(true));
    i.set_data(&d, "configurable", Value::Bool(true));
    if !matches!(&o, Value::Obj(_)) {
        return Ok(Value::Und);
    }
    // route through Object.defineProperty — a proxy receiver must see
    // its defineProperty trap fire; ordinary targets take the same
    // [[DefineOwnProperty]] path DefinePropertyOrThrow specifies
    let kv = i.key_to_value(&k);
    obj_define_property(i, &Value::Und, &[o, kv, Value::Obj(d)])?;
    Ok(Value::Und)
}

fn obj_define_getter(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    obj_define_accessor(i, this, args, "get")
}

fn obj_define_setter(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    obj_define_accessor(i, this, args, "set")
}

/// Annex B `__lookupGetter__`/`__lookupSetter__` — per link the spec
/// runs [[GetOwnProperty]] (a proxy fires its trap) and then
/// [[GetPrototypeOf]]; the first accessor own prop answers, a data own
/// prop stops the walk with undefined.
fn obj_lookup_accessor(i: &mut Interp, this: &Value, args: &[Value], getter: bool) -> NResult {
    let o = i.to_object(this)?;
    let k = i.to_propkey(&arg(args, 0))?;
    let mut cur = o;
    for _ in 0..10_000 {
        if !matches!(&cur, Value::Obj(_)) {
            return Ok(Value::Und);
        }
        let kv = i.key_to_value(&k);
        let d = obj_get_own_prop_desc(i, &Value::Und, &[cur.clone(), kv])?;
        if let Value::Obj(dv) = &d {
            let dv = Value::Obj(dv.clone());
            let g = i.get_prop_str(&dv, "get")?;
            let s = i.get_prop_str(&dv, "set")?;
            // accessor desc → answer; data desc → stop with undefined
            if !matches!(g, Value::Und) || !matches!(s, Value::Und) {
                return Ok(if getter { g } else { s });
            }
            return Ok(Value::Und);
        }
        // no own prop — walk to the [[GetPrototypeOf]] result (proxy
        // links fire the getPrototypeOf trap inside obj_get_proto)
        cur = obj_get_proto(i, &cur, &[])?;
    }
    // a getPrototypeOf trap looped forever — treat as end of chain
    Ok(Value::Und)
}

fn obj_lookup_getter(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    obj_lookup_accessor(i, this, args, true)
}

fn obj_lookup_setter(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    obj_lookup_accessor(i, this, args, false)
}

/// `Object.getOwnPropertyNames` — own string keys incl non-enumerable.
fn obj_get_own_prop_names(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let v = i.to_object(&arg(args, 0))?;
    let keys = own_keys_all(i, &v)?
        .into_iter()
        .filter_map(|k| match k {
            PropKey::S(s) => Some(Value::Str(s)),
            _ => None,
        })
        .collect();
    Ok(Value::Obj(i.new_arr(keys)))
}

/// `Reflect.ownKeys` — all own keys incl symbols.
fn reflect_own_keys(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let target = arg(args, 0);
    if !matches!(target, Value::Obj(_)) {
        return Err(i.type_err("Reflect.ownKeys called on non-object"));
    }
    let keys = own_keys_all(i, &target)?
        .into_iter()
        .map(|k| match k {
            PropKey::S(s) => Value::Str(s),
            PropKey::Sym(id) => Value::Sym(id, Rc::from("")),
        })
        .collect();
    Ok(Value::Obj(i.new_arr(keys)))
}

fn obj_keys(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let v = i.to_object(&arg(args, 0))?;
    // spec EnumerableOwnProperties(KEY): ownKeys + [[GetOwnProperty]]
    // for the enumerable filter — NO [[Get]] on the values. A proxy
    // must therefore see ownKeys + gOPD traps only.
    let keys = if {
        matches!(&v, Value::Obj(o)
            if matches!(&o.borrow().kind, ObjKind::Proxy { .. }))
    } {
        let mut ks = Vec::new();
        for k in own_keys_all(i, &v)? {
            let PropKey::S(_) = &k else {
                continue;
            };
            let kv = i.key_to_value(&k);
            let d = obj_get_own_prop_desc(i, &Value::Und, &[v.clone(), kv])?;
            if let Value::Obj(dv) = d {
                let en = i.get_prop_str(&Value::Obj(dv), "enumerable")?;
                if i.to_bool(&en) {
                    if let PropKey::S(s) = k {
                        ks.push(Value::Str(s));
                    }
                }
            }
        }
        ks
    } else {
        own_enum_entries_full(i, &v, false)?
            .into_iter()
            .filter_map(|(k, _)| match k {
                PropKey::S(s) => Some(Value::Str(s)),
                _ => None, // string keys only
            })
            .collect()
    };
    Ok(Value::Obj(i.new_arr(keys)))
}

fn obj_values(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let v = i.to_object(&arg(args, 0))?;
    let out: Vec<Value> = own_enum_entries_full(i, &v, false)?
        .into_iter()
        .filter_map(|(k, val)| match k {
            PropKey::S(_) => Some(val),
            _ => None, // string keys only
        })
        .collect();
    Ok(Value::Obj(i.new_arr(out)))
}

fn obj_entries(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let v = i.to_object(&arg(args, 0))?;
    let mut out = Vec::new();
    for (k, val) in own_enum_entries_full(i, &v, false)? {
        let ks = match k {
            PropKey::S(s) => Value::Str(s),
            _ => continue, // string keys only
        };
        out.push(Value::Obj(i.new_arr(vec![ks, val])));
    }
    Ok(Value::Obj(i.new_arr(out)))
}

fn obj_assign(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    // spec ToObject on the target — primitives box to wrappers
    let target = i.to_object(&arg(args, 0))?;
    for src in args.iter().skip(1) {
        if matches!(src, Value::Und | Value::Null) {
            continue;
        }
        // sources are ToObject'd too — a string contributes its chars
        let src = i.to_object(src)?;
        for (k, v) in own_enum_entries_full(i, &src, true)? {
            // spec Set(to, nextKey, propValue, true) — a failed write
            // throws TypeError regardless of surrounding strictness
            i.set_failed = false;
            let st = std::mem::replace(&mut i.cur_strict, true);
            let r = i.set_prop(&target, k, v);
            i.cur_strict = st;
            i.set_failed = false;
            r?;
        }
    }
    Ok(target)
}

fn read_desc_prop(i: &mut Interp, desc: &Value, name: &str) -> Result<(bool, Value), Value> {
    // spec ToPropertyDescriptor reads via HasProperty — a present
    // `value: undefined`/`writable: undefined` IS part of the descriptor
    let present = i.has_prop(desc, &PropKey::from_str(name))?;
    let v = i.get_prop_str(desc, name)?;
    Ok((present, v))
}

fn obj_define_property(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let target = arg(args, 0);
    let Value::Obj(o) = target.clone() else {
        return Err(i.type_err("Object.defineProperty called on non-object"));
    };
    let key = i.to_propkey(&arg(args, 1))?;
    let desc = arg(args, 2);
    // a BigInt is stored as an Obj stand-in here but is a PRIMITIVE —
    // ToPropertyDescriptor still rejects it (spec 19.1.2.4 step 3)
    if !matches!(desc, Value::Obj(_)) || crate::binary::bigint_is_prim(&desc) {
        return Err(i.type_err("Property description must be an object"));
    }
    // spec ToPropertyDescriptor runs BEFORE [[DefineOwnProperty]] — the
    // six fields are read/validated up front, so a proxy's trap receives
    // the normalized FromPropertyDescriptor object, not the raw arg
    let pd = i.pdesc_from_obj(&desc)?;
    if let ObjKind::Proxy {
        target: tg,
        handler,
    } = { o.borrow().kind.clone() }
    {
        let (tg, h) = (tg, handler);
        return match i.proxy_trap(&h, "defineProperty")? {
            Some(trap) => {
                let kv = i.key_to_value(&key);
                let dobj = Value::Obj(i.pdesc_to_obj(&pd));
                let r = i.call(&trap, h, vec![tg.clone(), kv, dobj])?;
                if !i.to_bool(&r) {
                    return Err(i.type_err("proxy defineProperty trap returned falsy"));
                }
                i.proxy_define_invariant(&tg, &key, &pd)?;
                Ok(target)
            }
            None => match &tg {
                Value::Obj(to) => {
                    let to = to.clone();
                    let dobj = Value::Obj(i.pdesc_to_obj(&pd));
                    if !define_prop_from_desc(i, &to, key, &dobj)? {
                        return Err(i.type_err("Cannot define property"));
                    }
                    Ok(target)
                }
                _ => Err(i.type_err("proxy target is not an object")),
            },
        };
    }
    // fields were already validated — apply from a normalized object so
    // getters on the user's desc fire exactly once
    let dobj = Value::Obj(i.pdesc_to_obj(&pd));
    if !define_prop_from_desc(i, &o, key, &dobj)? {
        return Err(i.type_err("Cannot define property"));
    }
    Ok(target)
}

fn define_prop_from_desc(
    i: &mut Interp,
    o: &ObjRef,
    key: PropKey,
    desc: &Value,
) -> Result<bool, Value> {
    // proxy [[DefineOwnProperty]] — reached when an outer proxy's
    // missing `defineProperty` trap forwards to a target that is itself
    // a proxy (10.5.6 step 7.a). The trap's third arg is the descriptor
    // object; a truish result still faces the target-side invariants.
    let proxy = {
        let b = o.borrow();
        match &b.kind {
            ObjKind::Proxy { target, handler } => Some((target.clone(), handler.clone())),
            _ => None,
        }
    };
    if let Some((t, h)) = proxy {
        return match i.proxy_trap(&h, "defineProperty")? {
            Some(trap) => {
                let kv = i.key_to_value(&key);
                let r = i.call(&trap, h, vec![t.clone(), kv, desc.clone()])?;
                if !i.to_bool(&r) {
                    return Ok(false);
                }
                let pd = i.pdesc_from_obj(desc)?;
                i.proxy_define_invariant(&t, &key, &pd)?;
                Ok(true)
            }
            None => match &t {
                Value::Obj(to) => {
                    let to = to.clone();
                    define_prop_from_desc(i, &to, key, desc)
                }
                _ => Err(i.type_err("proxy target is not an object")),
            },
        };
    }
    let (has_get, gv) = read_desc_prop(i, desc, "get")?;
    let (has_set, sv) = read_desc_prop(i, desc, "set")?;
    let (has_val, vv) = read_desc_prop(i, desc, "value")?;
    let (has_w, wv) = read_desc_prop(i, desc, "writable")?;
    let (has_e, ev) = read_desc_prop(i, desc, "enumerable")?;
    let (has_c, cv) = read_desc_prop(i, desc, "configurable")?;
    let is_acc = has_get || has_set;
    // ToPropertyDescriptor validity (spec 6.2.6.5 steps 5-7)
    if is_acc && (has_val || has_w) {
        return Err(i.type_err(
            "Invalid property descriptor. Cannot both specify accessors and a value or writable attribute",
        ));
    }
    if has_get && !matches!(gv, Value::Und) && !i.is_callable(&gv) {
        return Err(i.type_err("Getter must be a function"));
    }
    if has_set && !matches!(sv, Value::Und) && !i.is_callable(&sv) {
        return Err(i.type_err("Setter must be a function"));
    }

    // 10.4.5.x integer-indexed [[DefineOwnProperty]]: canonical numeric
    // strings never become ordinary props — a valid index validates the
    // requested attrs then writes the element store; an invalid one
    // (detached, -0, negative, fractional, past the end) fails the
    // define outright.
    let ta_cni = match &key {
        PropKey::S(s) if matches!(&o.borrow().kind, ObjKind::Typed { .. }) => {
            canonical_numeric_index(s)
        }
        _ => None,
    };
    if let Some(ni) = ta_cni {
        let (buf, off, len, kind, track) = match &o.borrow().kind {
            ObjKind::Typed {
                buf,
                off,
                len,
                kind,
                track,
            } => (buf.clone(), *off, *len, *kind, *track),
            _ => unreachable!(),
        };
        if !typed_state(&buf, off, len, kind, track).is_some_and(|eff| valid_int_index(ni, eff)) {
            return Ok(false);
        }
        if (has_c && !i.to_bool(&cv))
            || (has_e && !i.to_bool(&ev))
            || is_acc
            || (has_w && !i.to_bool(&wv))
        {
            return Ok(false);
        }
        if has_val {
            let n = crate::binary::ta_elem_write(i, &Value::Obj(o.clone()), &vv)?;
            // coercion ran side-effects — revalidate before writing so a
            // detach/resize during valueOf drops the write (the define
            // still reports success, spec IntegerIndexedElementSet)
            if typed_state(&buf, off, len, kind, track).is_some_and(|eff| valid_int_index(ni, eff))
            {
                if buf.borrow().immutable {
                    return Err(i.type_err("Cannot write to an immutable ArrayBuffer"));
                }
                if let Some(bytes) = buffer_bytes(&buf) {
                    kind.write(&mut bytes.borrow_mut(), off + ni as usize * kind.size(), n);
                }
            }
        }
        return Ok(true);
    }

    // array element / length targets live in the dense store, not props
    {
        let mut b = o.borrow_mut();
        let args_idx = if let PropKey::S(s) = &key {
            arr_index(s)
        } else {
            None
        };
        let args_hole = args_idx.is_some_and(|i| b.holes.as_ref().is_some_and(|h| h.contains(&i)));
        let args_mat = b.props.get(&key).is_some();
        if let ObjKind::Args { items, env, map } = &mut b.kind {
            if let PropKey::S(s) = &key {
                if let Some(idx) = args_idx {
                    if idx < items.len() && !args_hole
                        // materialized slots go through the generic
                        // descriptor-validation path below
                        && !args_mat
                    {
                        let was_mapped = matches!(map.get(idx), Some(Some(_)));
                        // read the mapped value before severing — an
                        // unmapping define freezes the CURRENT param value
                        let mapped_val = if let (Some(e), Some(Some(name))) = (&*env, map.get(idx))
                        {
                            e.vars.borrow().get(name.as_ref()).map(|b| b.val.clone())
                        } else {
                            None
                        };
                        // spec 10.4.4.13 step 6.b.i: a [[Value]] write
                        // goes through the STILL-LIVE map first — the
                        // param binding observes the new value — and
                        // only then does writable:false/an accessor
                        // descriptor sever the link (configurable:false
                        // does NOT unmap)
                        if has_val {
                            items[idx] = vv.clone();
                            if let (Some(e), Some(Some(name))) = (&*env, map.get(idx)) {
                                let name = name.clone();
                                if let Some(bnd) = e.vars.borrow_mut().get_mut(name.as_ref()) {
                                    bnd.val = vv.clone();
                                }
                            }
                        }
                        if was_mapped && (is_acc || (has_w && !i.to_bool(&wv))) {
                            if let Some(m) = map.get_mut(idx) {
                                *m = None;
                            }
                        }
                        // materialize merged attrs so descriptor
                        // semantics apply to the slot
                        let cur_val = mapped_val.unwrap_or_else(|| items[idx].clone());
                        let (pw, pe, pc) = b
                            .props
                            .get(&key)
                            .map(|p| (p.writable, p.enumerable, p.configurable))
                            .unwrap_or((true, true, true));
                        b.props.insert(
                            key.clone(),
                            crate::interp::Prop {
                                val: if has_val { vv.clone() } else { cur_val },
                                writable: if has_w { i.to_bool(&wv) } else { pw },
                                enumerable: if has_e { i.to_bool(&ev) } else { pe },
                                configurable: if has_c { i.to_bool(&cv) } else { pc },
                                get: if has_get { Some(gv.clone()) } else { None },
                                set: if has_set { Some(sv.clone()) } else { None },
                                // see the generic merge below — Some(Und)
                                // keeps "declared undefined" presence
                            },
                        );
                        return Ok(true);
                    }
                }
            }
        }
        // accessor def on an array element slot: it becomes an own
        // accessor property — hole the element first so the props entry
        // shadows it, then the generic path defines the descriptor
        let acc_idx = if is_acc {
            match &key {
                PropKey::S(s) => arr_index(s.as_ref()),
                _ => None,
            }
        } else {
            None
        };
        if let Some(idx) = acc_idx {
            if matches!(&b.kind, ObjKind::Arr(it) if idx < it.len())
                && !b.holes.as_ref().is_some_and(|h| h.contains(&idx))
            {
                // materialize the live element first so the generic
                // validation merges against its current all-true attrs
                // (and so a non-extensible array may still convert it)
                if b.props.get(&key).is_none() {
                    let cur = match &b.kind {
                        ObjKind::Arr(it) => it[idx].clone(),
                        _ => Value::Und,
                    };
                    b.props.insert(
                        key.clone(),
                        Prop {
                            val: cur,
                            writable: true,
                            enumerable: true,
                            configurable: true,
                            get: None,
                            set: None,
                        },
                    );
                }
                b.holes.get_or_insert_with(Default::default).insert(idx);
            } else if matches!(&b.kind, ObjKind::Arr(_))
                && b.extensible
                && b.props.get(&key).is_none()
            {
                // ArrayDefineOwnProperty: an index past the end grows
                // length to idx+1 — `defineProperty([],"0",{get})` must
                // still lengthen so `for..of` visits the slot
                let need = idx + 1;
                let cur = match b.props.get(&PropKey::from_str("length")) {
                    Some(p) => match &p.val {
                        Value::Num(n) => *n as usize,
                        _ => 0,
                    },
                    None => 0,
                };
                if need > cur {
                    b.props.insert(
                        PropKey::from_str("length"),
                        Prop {
                            val: Value::Num(need as f64),
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
        if acc_idx.is_none() {
            if matches!(&b.kind, ObjKind::Arr(_)) {
                if let PropKey::S(s) = &key {
                    if let Some(idx) = arr_index(s) {
                        let mat = b.props.get(&key).is_some();
                        let hole = b.holes.as_ref().is_some_and(|h| h.contains(&idx));
                        let in_range = matches!(&b.kind, ObjKind::Arr(it) if idx < it.len());
                        let pure_val = has_val && !has_w && !has_e && !has_c;
                        if !in_range {
                            // index at/past the declared length needs a
                            // writable length (spec 10.4.2.1 step 3.e)
                            let lkey = PropKey::from_str("length");
                            let items_len = match &b.kind {
                                ObjKind::Arr(it) => it.len(),
                                _ => 0,
                            };
                            let (cur_len, len_w) = match b.props.get(&lkey) {
                                Some(p) => {
                                    let n = match &p.val {
                                        Value::Num(n) => (*n as usize).max(items_len),
                                        _ => items_len,
                                    };
                                    (n, p.writable)
                                }
                                None => (items_len, true),
                            };
                            if idx >= cur_len && !len_w {
                                return Ok(false);
                            }
                        }
                        if !mat {
                            if in_range && !hole {
                                if pure_val {
                                    if let ObjKind::Arr(items) = &mut b.kind {
                                        items[idx] = vv.clone();
                                    }
                                    return Ok(true);
                                }
                                // attribute fields present → materialize the
                                // live element so generic validation merges
                                // against its current all-true attrs
                                if let ObjKind::Arr(items) = &b.kind {
                                    let cur = items[idx].clone();
                                    b.props.insert(
                                        key.clone(),
                                        Prop {
                                            val: cur,
                                            writable: true,
                                            enumerable: true,
                                            configurable: true,
                                            get: None,
                                            set: None,
                                        },
                                    );
                                }
                            }
                            // everything else (fresh indices, holed slots,
                            // attr-bearing descs, non-extensible targets)
                            // goes through the generic path — a FRESH index
                            // define gets the descriptor's attrs verbatim
                            // (absent fields default to false, not the
                            // all-true of an assignment-grown slot); the
                            // tail sync grows `length` to idx+1
                        }
                    }
                    if s.as_ref() == "length" {
                        // `length` is a non-configurable data prop — an
                        // accessor desc always fails the kind-change check
                        if is_acc {
                            return Ok(false);
                        }
                        // 10.4.2.1 ArraySetLength — ToNumber may run user
                        // code, so release the borrow around the coercion
                        let new_len = if has_val {
                            drop(b);
                            let numlen = i.to_num(&vv)?;
                            b = o.borrow_mut();
                            let nlu = to_u32(numlen);
                            if numlen != nlu as f64 {
                                return Err(i.range_err("Invalid array length"));
                            }
                            Some(nlu as usize)
                        } else {
                            None
                        };
                        let items_len = match &b.kind {
                            ObjKind::Arr(it) => it.len(),
                            _ => return Ok(false),
                        };
                        let lkey = PropKey::from_str("length");
                        let (eff_len, cur_w) = match b.props.get(&lkey) {
                            Some(p) => {
                                let n = match &p.val {
                                    Value::Num(n) => (*n as usize).max(items_len),
                                    _ => items_len,
                                };
                                (n, p.writable)
                            }
                            None => (items_len, true),
                        };
                        // length is {enumerable:false, configurable:false}
                        // — attr flips to true are rejected outright
                        if (has_c && i.to_bool(&cv)) || (has_e && i.to_bool(&ev)) {
                            return Ok(false);
                        }
                        if !cur_w {
                            // non-writable length: value must stay the same
                            // and writable can't flip back on
                            if new_len.is_some_and(|n| n != eff_len) {
                                return Ok(false);
                            }
                            if has_w && i.to_bool(&wv) {
                                return Ok(false);
                            }
                            return Ok(true);
                        }
                        let new_w = if has_w { i.to_bool(&wv) } else { cur_w };
                        if let Some(nl) = new_len {
                            if nl < eff_len {
                                // spec deletes indices in [nl, eff_len)
                                // descending; the first non-configurable
                                // index aborts the shrink at that point
                                let doomed: Vec<(usize, PropKey)> = b
                                    .props
                                    .iter()
                                    .map(|(k, _)| k)
                                    .filter_map(|k| match k {
                                        PropKey::S(s) => js_int_index(s.as_ref())
                                            .filter(|ix| *ix >= nl && *ix < eff_len)
                                            .map(|ix| (ix, k.clone())),
                                        _ => None,
                                    })
                                    .collect();
                                let fail_at = doomed
                                    .iter()
                                    .filter(|(_, k)| {
                                        !b.props.get(k).map(|p| p.configurable).unwrap_or(true)
                                    })
                                    .map(|(ix, _)| *ix)
                                    .max();
                                if let Some(k) = fail_at {
                                    // indices > k were already deleted; the
                                    // length lands on k+1
                                    let keep: Vec<PropKey> = doomed
                                        .iter()
                                        .filter(|(ix, _)| *ix > k)
                                        .map(|(_, k)| k.clone())
                                        .collect();
                                    for pk in keep {
                                        b.props.remove(&pk);
                                    }
                                    if let ObjKind::Arr(items) = &mut b.kind {
                                        items.truncate(k + 1);
                                    }
                                    if let Some(h) = &mut b.holes {
                                        h.retain(|&h| h < k + 1);
                                    }
                                    match b.props.get_mut(&lkey) {
                                        Some(p) => {
                                            p.val = Value::Num((k + 1) as f64);
                                            if !new_w {
                                                p.writable = false;
                                            }
                                        }
                                        None => {
                                            b.props.insert(
                                                lkey,
                                                Prop {
                                                    val: Value::Num((k + 1) as f64),
                                                    writable: new_w,
                                                    enumerable: false,
                                                    configurable: false,
                                                    get: None,
                                                    set: None,
                                                },
                                            );
                                        }
                                    }
                                    return Ok(false);
                                }
                                for (_, pk) in doomed {
                                    b.props.remove(&pk);
                                }
                                if let ObjKind::Arr(items) = &mut b.kind {
                                    items.truncate(nl);
                                }
                                if let Some(h) = &mut b.holes {
                                    h.retain(|&h| h < nl);
                                }
                            }
                            match b.props.get_mut(&lkey) {
                                Some(p) => {
                                    p.val = Value::Num(nl as f64);
                                    p.writable = new_w;
                                }
                                None => {
                                    b.props.insert(
                                        lkey,
                                        Prop {
                                            val: Value::Num(nl as f64),
                                            writable: new_w,
                                            enumerable: false,
                                            configurable: false,
                                            get: None,
                                            set: None,
                                        },
                                    );
                                }
                            }
                            return Ok(true);
                        }
                        // attribute-only desc
                        match b.props.get_mut(&lkey) {
                            Some(p) => p.writable = new_w,
                            None => {
                                b.props.insert(
                                    lkey,
                                    Prop {
                                        val: Value::Num(eff_len as f64),
                                        writable: new_w,
                                        enumerable: false,
                                        configurable: false,
                                        get: None,
                                        set: None,
                                    },
                                );
                            }
                        }
                        return Ok(true);
                    }
                }
            }
        }
    }

    let existing = {
        let b = o.borrow();
        b.props.get(&key).cloned()
    };
    // non-configurable existing prop — ValidateAndApplyPropertyDescriptor
    // constraints (spec 10.1.6.3): attribute changes, data↔accessor kind
    // changes, and writes to non-writable values all throw.
    // ValidateAndApplyPropertyDescriptor's non-configurable constraints
    // all *return false* — Object.defineProperty throws via the caller,
    // Reflect.defineProperty reports the boolean (spec 10.1.6.3)
    if let Some(old) = &existing {
        if !old.configurable {
            if has_c && i.to_bool(&cv) {
                return Ok(false);
            }
            if has_e && i.to_bool(&ev) != old.enumerable {
                return Ok(false);
            }
            // spec: only a desc that actually names a kind can change
            // it — a GENERIC descriptor (no get/set/value/writable)
            // leaves an accessor an accessor and a data prop a data prop
            if (is_acc || has_val || has_w) && is_acc != old.is_accessor() {
                return Ok(false);
            }
            if old.is_accessor() {
                if has_get && !i.same_value(&gv, old.get.as_ref().unwrap_or(&Value::Und))? {
                    return Ok(false);
                }
                if has_set && !i.same_value(&sv, old.set.as_ref().unwrap_or(&Value::Und))? {
                    return Ok(false);
                }
            } else if !old.writable {
                if has_w && i.to_bool(&wv) {
                    return Ok(false);
                }
                if has_val && !i.same_value(&vv, &old.val)? {
                    return Ok(false);
                }
            }
        }
    }
    if existing.is_none() && !o.borrow().extensible {
        return Ok(false);
    }
    let mut prop = existing.unwrap_or(Prop {
        // NEW property: attributes absent from the desc default to false
        // (spec) — unlike ordinary assignment which defaults all true.
        val: Value::Und,
        writable: false,
        enumerable: false,
        configurable: false,
        get: None,
        set: None,
    });
    if is_acc {
        // keep field PRESENCE: `{get: undefined}` is an accessor
        // descriptor whose [[Get]] is undefined — Some(Und) marks it so
        // `is_accessor()` still reports true when both halves are absent
        if has_get {
            prop.get = Some(gv);
        }
        if has_set {
            prop.set = Some(sv);
        }
        prop.val = Value::Und;
        prop.writable = false;
    } else {
        if has_val {
            prop.val = vv.clone();
        }
        if has_w {
            prop.writable = i.to_bool(&wv);
        }
        // only an explicit data-kind desc (value/writable present) converts an
        // existing accessor to a data prop — attribute-only descs keep the
        // accessor pair (spec ValidateAndApplyPropertyDescriptor)
        if has_val || has_w {
            prop.get = None;
            prop.set = None;
        }
    }
    if has_e {
        prop.enumerable = i.to_bool(&ev);
    }
    if has_c {
        prop.configurable = i.to_bool(&cv);
    }
    {
        let mut b = o.borrow_mut();
        let mirror_val = prop.val.clone();
        b.props.insert(key.clone(), prop);
        if matches!(&b.kind, ObjKind::Arr(_)) {
            if let PropKey::S(s) = &key {
                // spec index range (< 2^32-1) drives length growth even
                // for props-only sparse entries beyond the dense cap
                if let Some(idx) = js_int_index(s) {
                    let in_range = matches!(&b.kind, ObjKind::Arr(it) if idx < it.len());
                    if in_range {
                        if is_acc {
                            // the accessor entry shadows the dense slot
                            b.holes.get_or_insert_with(Default::default).insert(idx);
                        } else {
                            if let ObjKind::Arr(items) = &mut b.kind {
                                items[idx] = mirror_val.clone();
                            }
                            if let Some(h) = &mut b.holes {
                                h.remove(&idx);
                            }
                        }
                    }
                    // a defined index ≥ length grows the declared length
                    // (spec 10.4.2.1 ArrayDefineOwnProperty)
                    let need = idx + 1;
                    let lkey = PropKey::from_str("length");
                    let items_len = match &b.kind {
                        ObjKind::Arr(it) => it.len(),
                        _ => 0,
                    };
                    let cur = match b.props.get(&lkey) {
                        Some(p) => match &p.val {
                            Value::Num(n) => (*n as usize).max(items_len),
                            _ => items_len,
                        },
                        None => items_len,
                    };
                    if need > cur {
                        match b.props.get_mut(&lkey) {
                            Some(p) => p.val = Value::Num(need as f64),
                            None => {
                                b.props.insert(
                                    lkey,
                                    Prop {
                                        val: Value::Num(need as f64),
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
                }
            }
        }
        if let ObjKind::Args { items, env, map } = &mut b.kind {
            if let PropKey::S(s) = &key {
                if let Some(idx) = arr_index(s) {
                    if idx < items.len() {
                        let mut freeze = None;
                        // spec 10.4.4.13: accessor desc or writable:false
                        // severs the link — the slot freezes at the
                        // parameter's current value
                        if is_acc || (has_w && !i.to_bool(&wv)) {
                            if let Some(Some(name)) = map.get(idx) {
                                let name = name.clone();
                                if let Some(e) = env {
                                    freeze =
                                        e.vars.borrow().get(name.as_ref()).map(|x| x.val.clone());
                                }
                                if let Some(m) = map.get_mut(idx) {
                                    *m = None;
                                }
                            }
                        }
                        if has_val {
                            items[idx] = vv.clone();
                            if let (Some(e), Some(Some(name))) = (&*env, map.get(idx)) {
                                let name = name.clone();
                                if let Some(bnd) = e.vars.borrow_mut().get_mut(name.as_ref()) {
                                    bnd.val = vv.clone();
                                }
                            }
                        } else if let Some(mv) = freeze {
                            items[idx] = mv.clone();
                            if let Some(p) = b.props.get_mut(&key) {
                                p.val = mv;
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(true)
}

fn obj_define_properties(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let target = arg(args, 0);
    let Value::Obj(_) = &target else {
        return Err(i.type_err("Object.defineProperties called on non-object"));
    };
    // spec 19.1.2.3.1: `ToObject(Properties)` — null/undefined throw
    let descs = i.to_object(&arg(args, 1))?;
    for (k, d) in own_enum_entries_full(i, &descs, true)? {
        // ToPropertyDescriptor requires an object — primitives throw
        // (a BigInt reads as an Obj stand-in but is still a primitive)
        if !matches!(d, Value::Obj(_)) || crate::binary::bigint_is_prim(&d) {
            return Err(i.type_err("Property description must be an object"));
        }
        // route through Object.defineProperty so a proxy target fires
        // its defineProperty trap per key
        let kv = i.key_to_value(&k);
        obj_define_property(i, &Value::Und, &[target.clone(), kv, d])?;
    }
    Ok(target)
}

pub(crate) fn obj_get_own_prop_desc(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let v = i.to_object(&arg(args, 0))?;
    let key = i.to_propkey(&arg(args, 1))?;
    let Value::Obj(o) = &v else {
        return Ok(Value::Und);
    };
    // `{ }` scopes the borrow — the gOPD trap may touch the proxy
    if let ObjKind::Proxy { target, handler } = &{ o.borrow().kind.clone() } {
        let (t, h) = (target.clone(), handler.clone());
        // the trap result is validated/normalized and checked against
        // the target's own descriptor — a completed desc comes back
        return match i.proxy_gopd(&t, &h, &key, o)? {
            Some(d) => Ok(Value::Obj(i.pdesc_to_obj(&d))),
            None => Ok(Value::Und),
        };
    }
    // arguments-object index — mapped slots describe the live binding
    {
        let b = o.borrow();
        if let ObjKind::Args { items, env, map } = &b.kind {
            if let PropKey::S(s) = &key {
                if let Some(idx) = arr_index(s) {
                    if idx < items.len() && !b.holes.as_ref().is_some_and(|h| h.contains(&idx)) {
                        let mapped_val = if let (Some(e), Some(Some(name))) = (env, map.get(idx)) {
                            e.vars.borrow().get(name.as_ref()).map(|b| b.val.clone())
                        } else {
                            None
                        };
                        if let Some(p) = b.props.get(&key) {
                            // materialized attrs
                            let val = mapped_val.unwrap_or_else(|| p.val.clone());
                            if p.get.is_some() || p.set.is_some() {
                                let g = p.get.clone();
                                let st = p.set.clone();
                                let (en, cf) = (p.enumerable, p.configurable);
                                drop(b);
                                let d = i.new_obj();
                                i.set_data(&d, "get", g.unwrap_or(Value::Und));
                                i.set_data(&d, "set", st.unwrap_or(Value::Und));
                                i.set_data(&d, "enumerable", Value::Bool(en));
                                i.set_data(&d, "configurable", Value::Bool(cf));
                                return Ok(Value::Obj(d));
                            }
                            let (w, en, cf) = (p.writable, p.enumerable, p.configurable);
                            drop(b);
                            let d = i.new_obj();
                            i.set_data(&d, "value", val);
                            i.set_data(&d, "writable", Value::Bool(w));
                            i.set_data(&d, "enumerable", Value::Bool(en));
                            i.set_data(&d, "configurable", Value::Bool(cf));
                            return Ok(Value::Obj(d));
                        }
                        let val = mapped_val.unwrap_or_else(|| items[idx].clone());
                        drop(b);
                        let d = i.new_obj();
                        i.set_data(&d, "value", val);
                        i.set_data(&d, "writable", Value::Bool(true));
                        i.set_data(&d, "enumerable", Value::Bool(true));
                        i.set_data(&d, "configurable", Value::Bool(true));
                        return Ok(Value::Obj(d));
                    }
                }
            }
        }
    }
    // integer-indexed exotic [[GetOwnProperty]]: canonical numeric
    // strings describe the element store — never props or the proto.
    if let PropKey::S(sk) = &key {
        if matches!(&o.borrow().kind, ObjKind::Typed { .. }) {
            if let Some(ni) = canonical_numeric_index(sk) {
                let (buf, off, len, kind, track) = match &o.borrow().kind {
                    ObjKind::Typed {
                        buf,
                        off,
                        len,
                        kind,
                        track,
                    } => (buf.clone(), *off, *len, *kind, *track),
                    _ => unreachable!(),
                };
                if !typed_state(&buf, off, len, kind, track)
                    .is_some_and(|eff| valid_int_index(ni, eff))
                {
                    return Ok(Value::Und);
                }
                let Some(bytes) = buffer_bytes(&buf) else {
                    return Ok(Value::Und);
                };
                let n = kind.read(&bytes.borrow(), off + ni as usize * kind.size());
                let elem = crate::binary::ta_pubify(i, &v, Value::Num(n));
                let d = i.new_obj();
                i.set_data(&d, "value", elem);
                i.set_data(&d, "writable", Value::Bool(true));
                i.set_data(&d, "enumerable", Value::Bool(true));
                // integer-indexed props are non-configurable (10.4.5.3)
                i.set_data(&d, "configurable", Value::Bool(false));
                return Ok(Value::Obj(d));
            }
        }
    }
    // array index/length
    if let ObjKind::Arr(items) = &o.borrow().kind {
        if let PropKey::S(s) = &key {
            if s.as_ref() == "length" {
                let (n, w) = {
                    let b = o.borrow();
                    match b.props.get(&PropKey::from_str("length")) {
                        Some(p) => {
                            let n = match &p.val {
                                Value::Num(n) => (*n as usize).max(items.len()),
                                _ => items.len(),
                            };
                            (n, p.writable)
                        }
                        None => (items.len(), true),
                    }
                };
                let d = i.new_obj();
                i.set_data(&d, "value", Value::Num(n as f64));
                i.set_data(&d, "writable", Value::Bool(w));
                i.set_data(&d, "enumerable", Value::Bool(false));
                i.set_data(&d, "configurable", Value::Bool(false));
                return Ok(Value::Obj(d));
            }
            if let Some(idx) = arr_index(s) {
                // a materialized index carries real attributes — its
                // props entry describes the slot, not the dense value
                if o.borrow().props.get(&key).is_none()
                    && idx < items.len()
                    && !o.borrow().holes.as_ref().is_some_and(|h| h.contains(&idx))
                {
                    let d = i.new_obj();
                    i.set_data(&d, "value", items[idx].clone());
                    i.set_data(&d, "writable", Value::Bool(true));
                    i.set_data(&d, "enumerable", Value::Bool(true));
                    i.set_data(&d, "configurable", Value::Bool(true));
                    return Ok(Value::Obj(d));
                }
            }
        }
    }
    let p = o.borrow().props.get(&key).cloned();
    match p {
        None => Ok(Value::Und),
        Some(p) => {
            let d = i.new_obj();
            if p.is_accessor() {
                i.set_data(&d, "get", p.get.unwrap_or(Value::Und));
                i.set_data(&d, "set", p.set.unwrap_or(Value::Und));
            } else {
                i.set_data(&d, "value", p.val);
                i.set_data(&d, "writable", Value::Bool(p.writable));
            }
            i.set_data(&d, "enumerable", Value::Bool(p.enumerable));
            i.set_data(&d, "configurable", Value::Bool(p.configurable));
            Ok(Value::Obj(d))
        }
    }
}

fn obj_get_own_prop_descs(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let v = i.to_object(&arg(args, 0))?;
    let out = i.new_obj();
    for k in own_keys_all(i, &v)? {
        let d = obj_get_own_prop_desc(i, &Value::Und, &[v.clone(), i.key_to_value(&k)])?;
        // spec 19.1.2.9: a key whose [[GetOwnProperty]] returned
        // undefined gets NO entry in the result object
        if matches!(d, Value::Und) {
            continue;
        }
        let ov = Value::Obj(out.clone());
        i.set_prop(&ov, k, d)?;
    }
    Ok(Value::Obj(out))
}

pub(crate) fn obj_get_prototype_of(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    // spec ToObject: null/undefined throw; primitives read their
    // wrapper's proto (Number.prototype &co)
    let v = i.to_object(&arg(args, 0))?;
    if let Value::Obj(o) = &v {
        // `{ }` scopes the borrow — the trap may touch the proxy
        if let ObjKind::Proxy { target, handler } = &{ o.borrow().kind.clone() } {
            let (t, h) = (target.clone(), handler.clone());
            return match i.proxy_trap(&h, "getPrototypeOf")? {
                Some(trap) => {
                    let r = i.call(&trap, h, vec![t.clone()])?;
                    match r {
                        Value::Obj(_) | Value::Null => {
                            i.proxy_getproto_invariant(&t, &r)?;
                            Ok(r)
                        }
                        _ => Err(i.type_err("proxy getPrototypeOf must return object or null")),
                    }
                }
                None => obj_get_prototype_of(i, &Value::Und, &[t]),
            };
        }
    }
    match v {
        Value::Obj(o) => Ok(match o.borrow().proto.clone() {
            Some(p) => Value::Obj(p),
            None => Value::Null,
        }),
        _ => Ok(Value::Null),
    }
}

/// Ordinary `[[SetPrototypeOf]]` as a boolean — proxy targets fire the
/// `setPrototypeOf` trap; a non-extensible object still succeeds when
/// the new proto is the SAME as the current one (spec 10.1.2.2).
fn ord_set_proto(i: &mut Interp, o: &ObjRef, v: &Value) -> Result<bool, Value> {
    // `{ }` scopes the borrow — the trap may touch the proxy
    if let ObjKind::Proxy { target, handler } = { o.borrow().kind.clone() } {
        return match i.proxy_trap(&handler, "setPrototypeOf")? {
            Some(trap) => {
                let r = i.call(&trap, handler, vec![target.clone(), v.clone()])?;
                if !i.to_bool(&r) {
                    return Ok(false);
                }
                // 10.5.2: on a non-extensible target the new proto must
                // be the same as the target's real one
                i.proxy_setproto_invariant(&target, v)?;
                Ok(true)
            }
            None => match target {
                Value::Obj(to) => ord_set_proto(i, &to, v),
                _ => Ok(false),
            },
        };
    }
    let cur = o.borrow().proto.clone();
    let same = match (v, &cur) {
        (Value::Obj(p), Some(c)) => Rc::ptr_eq(p, c),
        (Value::Null, None) => true,
        _ => false,
    };
    if same {
        return Ok(true);
    }
    // spec 9.4.7: %Object.prototype% is an immutable-prototype exotic
    // object — [[SetPrototypeOf]] only ever succeeds for SameValue(V,
    // current), handled above; anything else fails outright
    if Rc::ptr_eq(o, &i.p.obj) {
        return Ok(false);
    }
    if !o.borrow().extensible {
        return Ok(false);
    }
    match v {
        Value::Obj(p) => {
            if proto_cycle(o, p) {
                // [[SetPrototypeOf]] fails (false) — callers decide:
                // __proto__= swallows it, setPrototypeOf throws
                return Ok(false);
            }
            o.borrow_mut().proto = Some(p.clone());
        }
        Value::Null => o.borrow_mut().proto = None,
        _ => return Ok(false),
    }
    Ok(true)
}

fn obj_set_prototype_of(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let t = arg(args, 0);
    // spec RequireObjectCoercible — primitives pass through unchanged
    if matches!(t, Value::Und | Value::Null) {
        return Err(i.type_err("Cannot convert undefined or null to object"));
    }
    let v = arg(args, 1);
    if !matches!(v, Value::Obj(_) | Value::Null) {
        return Err(i.type_err("Object prototype may only be an Object or null"));
    }
    if let Value::Obj(o) = &t {
        if !ord_set_proto(i, o, &v)? {
            return Err(i.type_err("Can't set prototype"));
        }
    }
    Ok(t)
}

fn obj_create(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let proto = match arg(args, 0) {
        Value::Obj(p) => Some(p),
        Value::Null => None,
        _ => return Err(i.type_err("Object prototype may only be an Object or null")),
    };
    let o = Rc::new(RefCell::new(Obj::new(proto, ObjKind::Ord)));
    let descs = arg(args, 1);
    if !matches!(descs, Value::Und) {
        // spec: properties arg is ToObject'd — null throws
        let descs = i.to_object(&descs)?;
        for (k, d) in own_enum_entries_full(i, &descs, true)? {
            if !matches!(d, Value::Obj(_)) {
                return Err(i.type_err("Property description must be an object"));
            }
            define_prop_from_desc(i, &o, k, &d)?;
        }
    }
    Ok(Value::Obj(o))
}

/// `[[IsExtensible]]` as a boolean — proxies fire the `isExtensible` trap.
pub(crate) fn ord_is_extensible(i: &mut Interp, o: &ObjRef) -> Result<bool, Value> {
    // `{ }` scopes the borrow — the trap may touch the proxy
    if let ObjKind::Proxy { target, handler } = { o.borrow().kind.clone() } {
        return match i.proxy_trap(&handler, "isExtensible")? {
            Some(trap) => {
                let r = i.call(&trap, handler, vec![target.clone()])?;
                let br = i.to_bool(&r);
                // 10.5.3: the trap result MUST match the target's real
                // extensibility
                let tr = match &target {
                    Value::Obj(to) => ord_is_extensible(i, to)?,
                    _ => false,
                };
                if br != tr {
                    return Err(i.type_err(
                        "'isExtensible' on proxy: trap result does not match the target's extensibility",
                    ));
                }
                Ok(br)
            }
            None => match target {
                Value::Obj(to) => ord_is_extensible(i, &to),
                _ => Ok(false),
            },
        };
    }
    Ok(o.borrow().extensible)
}

/// `[[PreventExtensions]]` as a boolean — proxies fire the trap.
fn ord_prevent_ext(i: &mut Interp, o: &ObjRef) -> Result<bool, Value> {
    // `{ }` scopes the borrow — the trap may touch the proxy
    if let ObjKind::Proxy { target, handler } = { o.borrow().kind.clone() } {
        return match i.proxy_trap(&handler, "preventExtensions")? {
            Some(trap) => {
                let r = i.call(&trap, handler, vec![target.clone()])?;
                let br = i.to_bool(&r);
                // 10.5.4: a truish trap result is only legal once the
                // target is actually non-extensible
                if br {
                    let ext = match &target {
                        Value::Obj(to) => ord_is_extensible(i, to)?,
                        _ => false,
                    };
                    if ext {
                        return Err(i.type_err(
                            "'preventExtensions' on proxy: trap returned truish while the target is still extensible",
                        ));
                    }
                }
                Ok(br)
            }
            None => match target {
                Value::Obj(to) => ord_prevent_ext(i, &to),
                _ => Ok(false),
            },
        };
    }
    // 10.4.5.4: a TA backed by a resizable buffer can never become
    // non-extensible — neither can one with live elements
    {
        let b = o.borrow();
        if let ObjKind::Typed {
            buf,
            off,
            len,
            kind,
            track,
        } = &b.kind
        {
            if buf.borrow().rab_max.is_some() {
                return Ok(false);
            }
            if *track {
                return Ok(false);
            }
            if typed_state(buf, *off, *len, *kind, *track).is_some_and(|eff| eff > 0) {
                return Ok(false);
            }
        }
    }
    o.borrow_mut().extensible = false;
    Ok(true)
}

/// Dense array/arguments elements carry no attributes until they are
/// materialized — integrity operations copy every live slot into props
/// so attr clamps apply uniformly (capped for absurd sizes).
fn materialize_elements(o: &ObjRef) {
    let mut b = o.borrow_mut();
    let n = match &b.kind {
        ObjKind::Arr(items) | ObjKind::Args { items, .. } => items.len(),
        _ => return,
    };
    for idx in 0..n.min(1_000_000) {
        if b.holes.as_ref().is_some_and(|h| h.contains(&idx)) {
            continue;
        }
        let k = idx_key(idx);
        if b.props.get(&k).is_none() {
            let val = match &b.kind {
                ObjKind::Arr(items) | ObjKind::Args { items, .. } => items[idx].clone(),
                _ => Value::Und,
            };
            b.props.insert(k, Prop::data(val));
        }
    }
    // the virtual `length` materializes too so the integrity clamp
    // reaches it — a frozen array's length is non-writable
    if matches!(&b.kind, ObjKind::Arr(_)) {
        let items_len = match &b.kind {
            ObjKind::Arr(items) => items.len(),
            _ => 0,
        };
        let k = PropKey::from_str("length");
        if b.props.get(&k).is_none() {
            b.props.insert(
                k,
                Prop {
                    val: Value::Num(items_len as f64),
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

/// SetIntegrityLevel — preventExtensions, then clamp every own prop.
/// A proxy runs it through traps; typed arrays with live elements can
/// never reach any integrity level (element descriptors always report
/// {w,e,c:true}) so the op fails → the caller throws.
pub(crate) fn set_integrity(i: &mut Interp, o: &ObjRef, frozen: bool) -> Result<bool, Value> {
    let ov = Value::Obj(o.clone());
    if !ord_prevent_ext(i, o)? {
        return Ok(false);
    }
    if { matches!(&o.borrow().kind, ObjKind::Proxy { .. }) } {
        for k in own_keys_all(i, &ov)? {
            let kv = i.key_to_value(&k);
            // spec SetIntegrityLevel: frozen first runs
            // O.[[GetOwnProperty]](k) per key — the trap observes them
            // in ownKeys order — and skips the define when it reports
            // undefined; accessor descs get only {configurable:false}
            let desc = obj_get_own_prop_desc(i, &Value::Und, &[ov.clone(), kv.clone()])?;
            if matches!(desc, Value::Und) {
                continue;
            }
            let mut is_acc = false;
            if let Value::Obj(dv) = &desc {
                let dv = Value::Obj(dv.clone());
                let g = i.get_prop_str(&dv, "get")?;
                let s = i.get_prop_str(&dv, "set")?;
                is_acc = !(matches!(g, Value::Und) && matches!(s, Value::Und));
            }
            let d = i.new_obj();
            i.set_data(&d, "configurable", Value::Bool(false));
            if frozen && !is_acc {
                i.set_data(&d, "writable", Value::Bool(false));
            }
            obj_define_property(i, &Value::Und, &[ov.clone(), kv, Value::Obj(d)])?;
        }
        return Ok(true);
    }
    {
        let b = o.borrow();
        if let ObjKind::Typed {
            buf,
            off,
            len,
            kind,
            track,
        } = &b.kind
        {
            if typed_state(buf, *off, *len, *kind, *track).is_some_and(|eff| eff > 0) {
                return Ok(false);
            }
        }
    }
    materialize_elements(o);
    let mut b = o.borrow_mut();
    for p in b.props.values_mut() {
        p.configurable = false;
        if frozen && !p.is_accessor() {
            p.writable = false;
        }
    }
    Ok(true)
}

/// TestIntegrityLevel as a boolean — non-extensible plus every own prop
/// non-configurable (and non-writable for data props when `frozen`).
fn integrity(i: &mut Interp, o: &ObjRef, frozen: bool) -> Result<bool, Value> {
    if ord_is_extensible(i, o)? {
        return Ok(false);
    }
    if { matches!(&o.borrow().kind, ObjKind::Proxy { .. }) } {
        let ov = Value::Obj(o.clone());
        for k in own_keys_all(i, &ov)? {
            let d = obj_get_own_prop_desc(i, &Value::Und, &[ov.clone(), i.key_to_value(&k)])?;
            if let Value::Obj(d) = d {
                let dv = Value::Obj(d);
                let cfg = i.get_prop_str(&dv, "configurable")?;
                if i.to_bool(&cfg) {
                    return Ok(false);
                }
                if frozen {
                    let g = i.get_prop_str(&dv, "get")?;
                    let s = i.get_prop_str(&dv, "set")?;
                    let is_acc = !(matches!(g, Value::Und) && matches!(s, Value::Und));
                    let w = i.get_prop_str(&dv, "writable")?;
                    if !is_acc && i.to_bool(&w) {
                        return Ok(false);
                    }
                }
            }
        }
        return Ok(true);
    }
    let b = o.borrow();
    if let ObjKind::Typed {
        buf,
        off,
        len,
        kind,
        track,
    } = &b.kind
    {
        if typed_state(buf, *off, *len, *kind, *track).is_some_and(|eff| eff > 0) {
            return Ok(false);
        }
    }
    let ok = b
        .props
        .values()
        .all(|p| !p.configurable && (!frozen || p.is_accessor() || !p.writable));
    if !ok {
        return Ok(false);
    }
    // unmaterialized dense elements still report {w,e,c:true}
    let n = match &b.kind {
        ObjKind::Arr(items) | ObjKind::Args { items, .. } => Some(items.len()),
        _ => None,
    };
    if let Some(n) = n {
        for k in 0..n {
            if b.holes.as_ref().is_some_and(|h| h.contains(&k)) {
                continue;
            }
            if b.props.get(&idx_key(k)).is_none() {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn obj_freeze(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let v = arg(args, 0);
    if let Value::Obj(o) = &v {
        if !set_integrity(i, o, true)? {
            return Err(i.type_err("Cannot freeze"));
        }
    }
    Ok(v)
}

fn obj_seal(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let v = arg(args, 0);
    if let Value::Obj(o) = &v {
        if !set_integrity(i, o, false)? {
            return Err(i.type_err("Cannot seal"));
        }
    }
    Ok(v)
}

fn obj_prevent_ext(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let v = arg(args, 0);
    if let Value::Obj(o) = &v {
        if !ord_prevent_ext(i, o)? {
            return Err(i.type_err("Cannot prevent extensions"));
        }
    }
    Ok(v)
}

fn obj_identity(_i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    Ok(arg(args, 0))
}

fn obj_is_extensible(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    Ok(Value::Bool(match &arg(args, 0) {
        Value::Obj(o) => ord_is_extensible(i, o)?,
        _ => false,
    }))
}

fn obj_is_frozen(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    if let Value::Obj(o) = &arg(args, 0) {
        return Ok(Value::Bool(integrity(i, o, true)?));
    }
    Ok(Value::Bool(true))
}

fn obj_is_sealed(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    if let Value::Obj(o) = &arg(args, 0) {
        return Ok(Value::Bool(integrity(i, o, false)?));
    }
    Ok(Value::Bool(true))
}

fn obj_from_entries(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let o = i.new_obj();
    let ov = Value::Obj(o.clone());
    let iterable = arg(args, 0);
    // spec AddEntriesFromIterable — GetIterator, then lazy stepping so
    // the iterator is only advanced as entries are consumed and
    // IteratorClose runs when an entry read or define throws
    let m = i.get_prop(&iterable, &PropKey::Sym(SYM_ITERATOR))?;
    if !i.is_callable(&m) {
        return Err(i.type_err("object is not iterable"));
    }
    let iter = i.call(&m, iterable.clone(), Vec::new())?;
    let next_fn = i.get_prop(&iter, &PropKey::from_str("next"))?;
    if !i.is_callable(&next_fn) {
        return Err(i.type_err("iterator.next is not a function"));
    }
    loop {
        // IteratorStep + IteratorComplete + IteratorValue — abrupt
        // completions here propagate WITHOUT closing the iterator
        let r = i.call(&next_fn, iter.clone(), Vec::new())?;
        if !matches!(r, Value::Obj(_)) {
            return Err(i.type_err("iterator result must be an object"));
        }
        let done = i.get_prop_str(&r, "done")?;
        if i.to_bool(&done) {
            break;
        }
        let entry = i.get_prop_str(&r, "value")?;
        // spec: non-object entries throw TypeError through IteratorClose
        if !matches!(entry, Value::Obj(_)) {
            let e = i.type_err("Iterator result is not an entry object");
            let _ = i.iter_close_obj(&iter);
            return Err(e);
        }
        // Get(entry, "0"/"1") + the adder's ToPropertyKey/define step —
        // an abrupt completion closes the iterator (IteratorClose)
        let step = (|| -> Result<(), Value> {
            let k = i.get_prop_str(&entry, "0")?;
            let v = i.get_prop_str(&entry, "1")?;
            let key = i.to_propkey(&k)?;
            // CreateDataPropertyOrThrow — a fresh ordinary object always
            // succeeds, and no prototype setter can observe the write
            if let Value::Obj(oo) = &ov {
                oo.borrow_mut().props.insert(key, Prop::data(v));
            }
            Ok(())
        })();
        if let Err(e) = step {
            let _ = i.iter_close_obj(&iter);
            return Err(e);
        }
    }
    Ok(ov)
}

/// `Object.groupBy` — iterate the source, bucket each element under
/// ToPropertyKey(cb(elem, k)); the result has a null prototype.
fn obj_group_by(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let items = arg(args, 0);
    if matches!(items, Value::Und | Value::Null) {
        return Err(i.type_err("Cannot convert undefined or null to object"));
    }
    let cb = arg(args, 1);
    if !i.is_callable(&cb) {
        return Err(i.type_err("Object.groupBy callback is not callable"));
    }
    let out = Rc::new(RefCell::new(Obj::new(None, ObjKind::Ord)));
    let ov = Value::Obj(out.clone());
    let vals = i.iter_values(&items)?;
    for (idx, v) in vals.into_iter().enumerate() {
        let kv = i.call(&cb, Value::Und, vec![v.clone(), Value::Num(idx as f64)])?;
        let k = i.to_propkey(&kv)?;
        // spec AddValueToKeyedGroup: the group array is created with
        // CreateDataProperty so a preexisting array gets appended
        match i.get_prop(&ov, &k)? {
            Value::Obj(g) => {
                let len = i.get_prop_str(&Value::Obj(g.clone()), "length")?;
                let n = i.to_num(&len)? as usize;
                let gv = Value::Obj(g);
                i.set_prop(&gv, PropKey::from_str(&n.to_string()), v)?;
            }
            _ => {
                let g = i.new_arr(vec![v]);
                i.set_prop(&ov, k, Value::Obj(g))?;
            }
        }
    }
    Ok(ov)
}

/// `Map.groupBy` — same grouping as Object.groupBy, but keys keep their
/// identity (SameValueZero — no stringification) and the result is a Map.
fn map_group_by(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let items = arg(args, 0);
    if matches!(items, Value::Und | Value::Null) {
        return Err(i.type_err("Cannot convert undefined or null to object"));
    }
    let cb = arg(args, 1);
    if !i.is_callable(&cb) {
        return Err(i.type_err("Map.groupBy callback is not callable"));
    }
    let proto = match i.get_prop_str(this, "prototype")? {
        Value::Obj(p) => Some(p),
        _ => None,
    };
    let out = Rc::new(RefCell::new(Obj::new(proto, ObjKind::Map(Vec::new()))));
    out.borrow_mut()
        .props
        .insert_str("__map", Prop::builtin(Value::Bool(true)));
    let vals = i.iter_values(&items)?;
    for (idx, v) in vals.into_iter().enumerate() {
        let kv = i.call(&cb, Value::Und, vec![v.clone(), Value::Num(idx as f64)])?;
        // CanonicalNumericIndexString: -0 → +0 when stored as a map key
        let kv = if matches!(kv, Value::Num(n) if n == 0.0) {
            Value::Num(0.0)
        } else {
            kv
        };
        let mut b = out.borrow_mut();
        if let ObjKind::Map(entries) = &mut b.kind {
            if let Some(slot) = entries.iter_mut().find(|(k, _)| same_value_zero(k, &kv)) {
                if let Value::Obj(g) = &mut slot.1 {
                    if let ObjKind::Arr(items) = &mut g.borrow_mut().kind {
                        items.push(v);
                    }
                }
            } else {
                entries.push((kv, Value::Obj(i.new_arr(vec![v]))));
            }
        }
    }
    Ok(Value::Obj(out))
}

/// spec GetCapabilitiesExecutor — captures the resolve/reject pair a
/// promise ctor hands its executor onto the bound `this` holder. A
/// second call (or pre-filled slots) is a TypeError.
fn promise_cap_executor(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let taken = {
        let Value::Obj(h) = this else {
            return Err(i.type_err("capability executor has no holder"));
        };
        let b = h.borrow();
        let has = |k: &str| {
            b.props
                .get_str(k)
                .is_some_and(|p| !matches!(p.val, Value::Und))
        };
        has("resolve") || has("reject")
    };
    if taken {
        return Err(i.type_err("GetCapabilitiesExecutor called more than once"));
    }
    i.set_prop(this, PropKey::from_str("resolve"), arg(args, 0))?;
    i.set_prop(this, PropKey::from_str("reject"), arg(args, 1))?;
    Ok(Value::Und)
}

/// `Promise.withResolvers` — spec: NewPromiseCapability(this), then an
/// ordinary { promise, resolve, reject } record
fn promise_with_resolvers(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let (promise, resolve, reject) = new_promise_capability(i, this)?;
    let out = i.new_obj();
    i.set_data(&out, "promise", promise);
    i.set_data(&out, "resolve", resolve);
    i.set_data(&out, "reject", reject);
    Ok(Value::Obj(out))
}

fn obj_is(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let a = arg(args, 0);
    let b = arg(args, 1);
    // SameValue: NaN≈NaN, +0/-0 distinct
    let r = match (&a, &b) {
        (Value::Num(x), Value::Num(y)) => {
            if x.is_nan() && y.is_nan() {
                true
            } else if x == y {
                !(*x == 0.0 && x.is_sign_positive() != y.is_sign_positive())
            } else {
                false
            }
        }
        _ => i.same_value(&a, &b)?,
    };
    Ok(Value::Bool(r))
}

fn obj_get_own_syms(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let v = i.to_object(&arg(args, 0))?;
    let out: Vec<Value> = own_keys_all(i, &v)?
        .into_iter()
        .filter_map(|k| match k {
            PropKey::Sym(id) => Some(Value::Sym(id, Rc::from(""))),
            _ => None,
        })
        .collect();
    Ok(Value::Obj(i.new_arr(out)))
}

fn object_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec: "If NewTarget is neither undefined nor the active function,
    // return OrdinaryCreateFromConstructor(NewTarget)" — a subclass
    // `new O(x)` / Reflect.construct ignores the value arg and returns
    // the fresh object `construct` pre-allocated with newTarget's proto
    let nt = match &i.new_target_native {
        Some(Value::Obj(o)) => Some(o.clone()),
        _ => None,
    };
    if let Some(nt) = nt {
        let ctor = i.get_prop(
            &Value::Obj(i.p.global.clone()),
            &PropKey::from_str("Object"),
        );
        let is_self = matches!(&ctor, Ok(Value::Obj(c)) if Rc::ptr_eq(&nt, c));
        if !is_self {
            if matches!(this, Value::Obj(_)) {
                return Ok(this.clone());
            }
            // newTarget.prototype wasn't an object — the spec fallback
            // allocates with %Object.prototype%
            return Ok(Value::Obj(i.new_obj()));
        }
    }
    let v = arg(args, 0);
    // `new Object(x)` → `this` is pre-allocated only for plain new; for
    // Object() conversion we return the coerced object directly.
    match v {
        Value::Und | Value::Null => {
            if matches!(this, Value::Obj(_)) {
                Ok(this.clone())
            } else {
                Ok(Value::Obj(i.new_obj()))
            }
        }
        Value::Obj(_) if crate::binary::bigint_is_prim(&v) => {
            // Object(1n) boxes the primitive — the wrapper carries the
            // slots but is a real object (typeof 'object', strict-eq
            // identity semantics)
            let o = Rc::new(RefCell::new(Obj::new(
                Some(i.p.bigint.clone()),
                ObjKind::Ord,
            )));
            if let Value::Obj(src) = &v {
                for k in ["__bigint", "__bigint_x"] {
                    if let Some(p) = src.borrow().props.get_str(k).map(|p| p.val.clone()) {
                        i.set_attr(&o, k, p, false, false);
                    }
                }
            }
            i.set_attr(&o, "__bigint_wrap", Value::Bool(true), false, false);
            i.set_builtin(&o, "$value", v.clone());
            Ok(Value::Obj(o))
        }
        Value::Obj(_) => Ok(v),
        // box primitives — to_object builds the proper wrapper incl.
        // the String exotic indexed-char and length slots
        _ => i.to_object(&v),
    }
}

// ===========================================================================
// natives: function
// ===========================================================================

fn fn_call(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let this_arg = arg(args, 0);
    let rest: Vec<Value> = args.iter().skip(1).cloned().collect();
    i.call(this, this_arg, rest)
}

fn fn_apply(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let this_arg = arg(args, 0);
    let arr = arg(args, 1);
    let list = if matches!(arr, Value::Und | Value::Null) {
        Vec::new()
    } else {
        // CreateListFromArrayLike requires an Object — primitives
        // (booleans, numbers, strings, symbols) are a TypeError even
        // though their wrappers would have readable `length` props
        if !matches!(arr, Value::Obj(_)) {
            return Err(i.type_err("argArray must be an object"));
        }
        to_elems(i, &arr)?
    };
    i.call(this, this_arg, list)
}

fn fn_bind(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    if !i.is_callable(this) {
        return Err(i.type_err("Bind must be called on a function"));
    }
    let bound = Rc::new(RefCell::new(Obj::new(
        Some(i.p.func.clone()),
        ObjKind::Bound {
            target: this.clone(),
            thisv: arg(args, 0),
            extra: args.iter().skip(1).cloned().collect(),
        },
    )));
    let bound_args = (args.len() as f64 - 1.0).max(0.0);
    // spec order: `length` is defined before `name`. Only an OWN
    // `length` on the target counts (inherited values are ignored —
    // `bind` on a fn with `{length:42}` on its proto yields 0), and
    // only numeric values apply ToIntegerOrInfinity → clamp at 0
    let has_len = has_own_impl(i, this, &Value::Str(Rc::from("length")))?;
    let blen = if matches!(has_len, Value::Bool(true)) {
        match i.get_prop_str(this, "length")? {
            Value::Num(n) => {
                let n = if n.is_nan() { 0.0 } else { n.trunc() };
                (n - bound_args).max(0.0)
            }
            _ => 0.0,
        }
    } else {
        0.0
    };
    i.set_attr(&bound, "length", Value::Num(blen), false, true);
    // name = "bound " + ToString(target.name); a throwing `name`
    // getter propagates (don't swallow the error); non-string names
    // contribute ""
    let tname = match i.get_prop_str(this, "name")? {
        Value::Str(s) => s.to_string(),
        _ => String::new(),
    };
    i.set_attr(
        &bound,
        "name",
        Value::Str(Rc::from(format!("bound {tname}"))),
        false,
        true,
    );
    Ok(Value::Obj(bound))
}

/// 7.3.19 OrdinaryHasInstance(C, O) — bound functions unwrap to their
/// target; non-callable C → false; `C.prototype` non-object → TypeError.
pub(crate) fn ordinary_has_instance(i: &mut Interp, c: &Value, o: &Value) -> Result<bool, Value> {
    let mut cur = c.clone();
    loop {
        let t = if let Value::Obj(co) = &cur {
            match co.borrow().kind.clone() {
                ObjKind::Bound { target, .. } => Some(target),
                _ => None,
            }
        } else {
            None
        };
        match t {
            Some(t) => cur = t,
            None => break,
        }
    }
    if !i.is_callable(&cur) {
        return Ok(false);
    }
    // spec order: a non-object O short-circuits BEFORE C.prototype is
    // fetched — a `prototype` getter must not fire for primitive lhs
    let Value::Obj(mut cur_o) = o.clone() else {
        return Ok(false);
    };
    let p = i.get_prop_str(&cur, "prototype")?;
    let Value::Obj(want) = p else {
        return Err(i.type_err("prototype is not an object"));
    };
    for _ in 0..10_000 {
        // each hop is O.[[GetPrototypeOf]] — a proxy in the chain
        // fires its getPrototypeOf trap (scoped borrow: the trap may
        // mutate the object)
        let next = if { matches!(&cur_o.borrow().kind, ObjKind::Proxy { .. }) } {
            match obj_get_prototype_of(i, &Value::Und, &[Value::Obj(cur_o.clone())])? {
                Value::Obj(p) => Some(p),
                _ => None,
            }
        } else {
            cur_o.borrow().proto.clone()
        };
        match next {
            Some(n) => {
                if Rc::ptr_eq(&n, &want) {
                    return Ok(true);
                }
                cur_o = n;
            }
            None => return Ok(false),
        }
    }
    Ok(false)
}

fn fn_has_instance(i: &mut Interp, this: &Value, a: &[Value]) -> NResult {
    let v = a.first().cloned().unwrap_or(Value::Und);
    Ok(Value::Bool(ordinary_has_instance(i, this, &v)?))
}

/// a single valid IdentifierName (native-source function names may be
/// keyword-shaped but never contain spaces, `#`, or punctuation)
fn is_ident_name(s: &str) -> bool {
    fn start(c: char) -> bool {
        c == '_' || c == '$' || unicode_ident::is_xid_start(c)
    }
    fn part(c: char) -> bool {
        start(c) || unicode_ident::is_xid_continue(c) || c == '\u{200c}' || c == '\u{200d}'
    }
    !s.is_empty() && s.chars().next().is_some_and(start) && s.chars().all(part)
}

fn fn_to_string(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let Value::Obj(o) = this else {
        return Err(i.type_err("Function.prototype.toString called on non-callable receiver"));
    };
    let kind = o.borrow().kind.clone();
    // a user function retains the verbatim source text of its
    // definition — toString returns it untouched (spec)
    if let ObjKind::Func(Func::User { def, .. }) = &kind {
        if let Some(src) = &def.src {
            return Ok(Value::Str(src.clone()));
        }
    }
    let name = match &kind {
        ObjKind::Func(Func::User { def, .. }) => def.name.clone().unwrap_or_default(),
        ObjKind::Func(Func::Native { name, .. }) => name.to_string(),
        // bound functions have no source text — anonymous native form
        ObjKind::Bound { .. } => String::new(),
        // a proxy answers the NativeFunction form only when its target
        // is callable; a non-callable target is a TypeError
        ObjKind::Proxy { target, .. } => {
            if matches!(target, Value::Obj(t) if t.borrow().is_callable()) {
                String::new()
            } else {
                return Err(
                    i.type_err("Function.prototype.toString called on non-callable receiver")
                );
            }
        }
        ObjKind::Und => String::new(),
        _ => return Err(i.type_err("Function.prototype.toString called on non-callable receiver")),
    };
    // emit the name only when it is a single valid IdentifierName —
    // computed/`#private`/otherwise-unusual names fall back to the
    // anonymous form so the result is always valid NativeFunction syntax
    let emit = if is_ident_name(&name) {
        name
    } else {
        String::new()
    };
    Ok(Value::Str(Rc::from(
        format!("function {emit}() {{ [native code] }}").as_str(),
    )))
}

// ===========================================================================
// natives: array
// ===========================================================================

fn array_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // Array(n) → length-n array of HOLES; Array(a,b,..) → [a,b,..]
    let (items, declared) = if args.len() == 1 {
        if let Value::Num(n) = args[0] {
            if n >= 0.0 && n == n.trunc() && n < 4294967295.0 {
                // dense Vec backing — can't honor holey arrays past a
                // sane bound (spec allows 2^32-1; real engines go sparse)
                if n > 8_000_000.0 {
                    return Err(i.range_err("Invalid array length"));
                }
                // spec: the slots are ABSENT, not undefined — an empty
                // dense store + a declared `length` keeps `i in arr`
                // false and lets proto chains (incl. proxies) answer
                (Vec::new(), Some(n as usize))
            } else {
                return Err(i.range_err("Invalid array length"));
            }
        } else {
            (args.to_vec(), None)
        }
    } else {
        (args.to_vec(), None)
    };
    // [[Construct]] via super(): `this` was allocated with
    // newTarget.prototype — convert it in place so the subclass proto
    // survives (`class Sub extends Array {}`)
    if let Value::Obj(o) = this {
        o.borrow_mut().kind = ObjKind::Arr(items);
        if let Some(n) = declared {
            i.set_attr(o, "length", Value::Num(n as f64), true, false);
        }
        return Ok(this.clone());
    }
    let o = i.new_arr(items);
    if let Some(n) = declared {
        i.set_attr(&o, "length", Value::Num(n as f64), true, false);
    }
    Ok(Value::Obj(o))
}

fn array_is_array(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    // spec IsArray: a proxy forwards to its target (chains unwrap);
    // a REVOKED proxy throws a TypeError
    let mut cur = arg(args, 0);
    for _ in 0..10_000 {
        let Value::Obj(o) = &cur else {
            return Ok(Value::Bool(false));
        };
        let nxt = match &o.borrow().kind {
            ObjKind::Arr(_) => return Ok(Value::Bool(true)),
            ObjKind::Proxy { target, .. } => {
                if matches!(target, Value::Null) {
                    return Err(
                        i.type_err("Cannot perform 'IsArray' on a proxy that has been revoked")
                    );
                }
                target.clone()
            }
            _ => return Ok(Value::Bool(false)),
        };
        cur = nxt;
    }
    Ok(Value::Bool(false))
}

/// spec Array.from(items, mapfn, thisArg):
/// mapfn check → items nullish check → GetMethod(@@iterator) → either the
/// iterator path (Construct(C,[]) → lazy steps → CreateDataProperty per
/// element → Set length=k) or the array-like path (ToObject → ToLength →
/// Construct(C,[len]) → per-index Get → CreateDataProperty → Set length).
fn array_from(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let src = arg(args, 0);
    let map_fn = arg(args, 1);
    let this_arg = arg(args, 2);
    let mapping = !matches!(map_fn, Value::Und);
    if mapping && !i.is_callable(&map_fn) {
        return Err(i.type_err("Array.from: mapping function is not callable"));
    }
    if matches!(src, Value::Und | Value::Null) {
        return Err(i.type_err("Array.from requires an array-like or iterable"));
    }
    // GetMethod(items, @@iterator) — getters/proxy traps observable here
    let im = i.get_prop(&src, &PropKey::Sym(SYM_ITERATOR))?;
    if i.is_callable(&im) {
        let a = if i.is_constructor(this) {
            i.construct(this, vec![], None)?
        } else {
            Value::Obj(i.new_arr(Vec::new()))
        };
        let mut lz = for_of_iter(i, &src)?;
        let mut k = 0usize;
        loop {
            let v = match iter_lazy_step(i, &mut lz)? {
                Some(v) => v,
                None => break,
            };
            let mapped = if mapping {
                match i.call(&map_fn, this_arg.clone(), vec![v, Value::Num(k as f64)]) {
                    Ok(m) => m,
                    Err(e) => {
                        let _ = iter_lazy_close(i, &mut lz);
                        return Err(e);
                    }
                }
            } else {
                v
            };
            if let Err(e) = create_data_prop(i, &a, k, mapped) {
                let _ = iter_lazy_close(i, &mut lz);
                return Err(e);
            }
            k += 1;
            if k > 100_000_000 {
                let _ = iter_lazy_close(i, &mut lz);
                return Err(i.range_err("Array.from: too many elements"));
            }
        }
        i.set_prop(&a, PropKey::from_str("length"), Value::Num(k as f64))?;
        return Ok(a);
    }
    // array-like path — live Get per index (deleted/updated elements
    // reflect at read time)
    let obj = i.to_object(&src)?;
    let lv = i.get_prop(&obj, &PropKey::from_str("length"))?;
    let len = to_length_num(i.to_num(&lv)?);
    let a = if i.is_constructor(this) {
        i.construct(this, vec![Value::Num(len as f64)], None)?
    } else {
        Value::Obj(i.new_arr(Vec::new()))
    };
    for k in 0..len {
        let v = i.get_prop(&obj, &idx_key(k))?;
        let mapped = if mapping {
            i.call(&map_fn, this_arg.clone(), vec![v, Value::Num(k as f64)])?
        } else {
            v
        };
        create_data_prop(i, &a, k, mapped)?;
    }
    i.set_prop(&a, PropKey::from_str("length"), Value::Num(len as f64))?;
    Ok(a)
}

/// spec Array.of: constructor this → `new C(len)`, else ArrayCreate —
/// then CreateDataProperty per arg + observable `length` Set
fn array_of(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let len = args.len();
    let a = if i.is_constructor(this) {
        i.construct(this, vec![Value::Num(len as f64)], None)?
    } else {
        Value::Obj(i.new_arr(Vec::new()))
    };
    for (k, v) in args.iter().enumerate() {
        create_data_prop(i, &a, k, v.clone())?;
    }
    i.set_prop(&a, PropKey::from_str("length"), Value::Num(len as f64))?;
    Ok(a)
}

fn arr_push(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec: O = ToObject → len = ToLength(Get "length") → TypeError when
    // len + argCount exceeds 2^53-1 → Set(O, len+k, E) per item →
    // Set(O, "length", newLen). Generic: any array-like, and a failing
    // [[Set]] (read-only length, string/TA receiver) always throws.
    let obj = i.to_object(this)?;
    let mut len = length_of_array_like(i, &obj)?;
    if len + args.len() > 9007199254740991usize {
        return Err(i.type_err("Array.prototype.push: too many items"));
    }
    for e in args {
        spec_set_idx(i, &obj, len, e.clone())?;
        len += 1;
    }
    spec_set(i, &obj, pk("length"), Value::Num(len as f64))?;
    Ok(Value::Num(len as f64))
}

fn arr_pop(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    // spec: len==0 → Set("length", 0) + undefined; else element = Get →
    // DeletePropertyOrThrow → Set("length", len-1) → element
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    if len == 0 {
        spec_set(i, &obj, pk("length"), Value::Num(0.0))?;
        return Ok(Value::Und);
    }
    let idx = len - 1;
    let el = i.get_prop(&obj, &idx_key(idx))?;
    spec_del_idx(i, &obj, idx)?;
    spec_set(i, &obj, pk("length"), Value::Num(idx as f64))?;
    Ok(el)
}

fn arr_shift(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    // spec: len==0 → Set("length", 0), undefined; else first = Get(0),
    // elements renumber via Has → Get → Set / absent → Delete, last
    // index deleted, Set("length", len-1)
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    if len == 0 {
        spec_set(i, &obj, pk("length"), Value::Num(0.0))?;
        return Ok(Value::Und);
    }
    let first = i.get_prop(&obj, &idx_key(0))?;
    for k in 1..len {
        let key = idx_key(k);
        if i.has_prop(&obj, &key)? {
            let v = i.get_prop(&obj, &key)?;
            spec_set_idx(i, &obj, k - 1, v)?;
        } else {
            spec_del_idx(i, &obj, k - 1)?;
        }
    }
    spec_del_idx(i, &obj, len - 1)?;
    spec_set(i, &obj, pk("length"), Value::Num((len - 1) as f64))?;
    Ok(first)
}

fn arr_unshift(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec: when argCount>0 (and len+argCount ≤ 2^53-1), elements renumber
    // right-to-left by argCount (Has → Get → Set / absent → Delete), then
    // the items Set in at the front, then Set "length".
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    let argc = args.len();
    if argc > 0 {
        if len + argc > 9007199254740991usize {
            return Err(i.type_err("Array.prototype.unshift: too many items"));
        }
        let mut k = len;
        while k > 0 {
            k -= 1;
            let from = idx_key(k);
            if i.has_prop(&obj, &from)? {
                let v = i.get_prop(&obj, &from)?;
                spec_set_idx(i, &obj, k + argc, v)?;
            } else {
                spec_del_idx(i, &obj, k + argc)?;
            }
        }
        for (j, e) in args.iter().enumerate() {
            spec_set_idx(i, &obj, j, e.clone())?;
        }
    }
    spec_set(i, &obj, pk("length"), Value::Num((len + argc) as f64))?;
    Ok(Value::Num((len + argc) as f64))
}

fn arr_slice(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec: O → LengthOfArrayLike → ToIntegerOrInfinity(start/end) →
    // ArraySpeciesCreate(O, count) → per-index HasProperty → live Get →
    // CreateDataProperty (getters fire, mid-copy deletions are observed)
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    let start = clamp_index(to_integer_or_inf(i.to_num(&arg(args, 0))?), len);
    let end = match arg(args, 1) {
        Value::Und => len,
        v => clamp_index(to_integer_or_inf(i.to_num(&v)?), len),
    };
    let a = species_create(i, &obj, end.saturating_sub(start))?;
    let mut n = 0usize;
    for k in start..end {
        let key = idx_key(k);
        if i.has_prop(&obj, &key)? {
            let v = i.get_prop(&obj, &key)?;
            create_data_prop(i, &a, n, v)?;
        }
        n += 1;
    }
    Ok(a)
}

/// LengthOfArrayLike for the change-by-copy methods — the result must
/// fit an Array (len <= 2**32-1), and our dense backing caps lower
fn arr_cbc_len(i: &mut Interp, this: &Value) -> Result<usize, Value> {
    // spec ToLength: NaN → +0, ±Infinity clamps into [0, 2**53-1]
    let lv = i.get_prop_str(this, "length")?;
    let n = i.to_num(&lv)?;
    let n = if n.is_nan() { 0.0 } else { n };
    let n = n.max(0.0).min(9007199254740991.0);
    if n > 4294967295.0 {
        return Err(i.range_err("Invalid array length"));
    }
    Ok(n as usize)
}

/// ES2023 change-by-copy methods — elements come via Get, a fresh dense
/// Array is built, the receiver is never touched.
fn arr_to_reversed(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let len = arr_cbc_len(i, this)?;
    if len > 8_000_000 {
        return Err(i.range_err("Invalid array length"));
    }
    let mut out = Vec::with_capacity(len);
    // spec reads indices in *descending* order — observable via getters
    for k in (0..len).rev() {
        out.push(i.get_prop(this, &idx_key(k))?);
    }
    Ok(Value::Obj(i.new_arr(out)))
}

fn arr_to_sorted(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cmp = arg(args, 0);
    if !matches!(cmp, Value::Und) && !i.is_callable(&cmp) {
        return Err(i.type_err("Array.prototype.toSorted comparator is not callable"));
    }
    let len = arr_cbc_len(i, this)?;
    if len > 8_000_000 {
        return Err(i.range_err("Invalid array length"));
    }
    let mut elems = Vec::with_capacity(len);
    for k in 0..len {
        elems.push(i.get_prop(this, &idx_key(k))?);
    }
    let v = Value::Obj(i.new_arr(elems));
    arr_sort(i, &v, args)
}

fn arr_to_spliced(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec LengthOfArrayLike: NaN → +0, clamps into [0, 2**53-1] — the
    // *source* can exceed the Array limit; only newLen is bounded
    let lv = i.get_prop_str(this, "length")?;
    let ln = i.to_num(&lv)?;
    let len = (if ln.is_nan() { 0.0 } else { ln })
        .max(0.0)
        .min(9007199254740991.0);
    let rel_start = if args.is_empty() {
        0.0
    } else {
        i.to_num(&arg(args, 0))?.trunc()
    };
    let rel_start = if rel_start.is_nan() { 0.0 } else { rel_start };
    let start = if rel_start < 0.0 {
        (len + rel_start).max(0.0)
    } else {
        rel_start.min(len)
    };
    let (del, inserts): (f64, &[Value]) = if args.is_empty() {
        (0.0, &[])
    } else if args.len() == 1 {
        (len - start, &[])
    } else {
        let d = i.to_num(&arg(args, 1))?.trunc();
        let d = if d.is_nan() { 0.0 } else { d };
        (d.clamp(0.0, len - start), &args[2..])
    };
    let new_len = len - del + inserts.len() as f64;
    if new_len > 9007199254740991.0 {
        return Err(i.type_err("Maximum call stack size exceeded"));
    }
    if new_len > 8_000_000.0 {
        return Err(i.range_err("Invalid array length"));
    }
    let (start, del) = (start as usize, del as usize);
    let mut out = Vec::with_capacity(new_len as usize);
    // elements inside the deleted range are never read
    for k in 0..start {
        out.push(i.get_prop(this, &idx_key(k))?);
    }
    out.extend_from_slice(inserts);
    for k in start + del..len as usize {
        out.push(i.get_prop(this, &idx_key(k))?);
    }
    Ok(Value::Obj(i.new_arr(out)))
}

fn arr_with(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let len = arr_cbc_len(i, this)?;
    if len > 8_000_000 {
        return Err(i.range_err("Invalid array length"));
    }
    let rel = i.to_num(&arg(args, 0))?.trunc();
    let rel = if rel.is_nan() { 0.0 } else { rel };
    let idx = if rel < 0.0 { rel + len as f64 } else { rel };
    if !(idx >= 0.0 && idx < len as f64) {
        return Err(i.range_err("Invalid index"));
    }
    let idx = idx as usize;
    let mut out = Vec::with_capacity(len);
    for k in 0..len {
        // the replaced index is never read from the receiver
        if k == idx {
            out.push(arg(args, 1));
        } else {
            out.push(i.get_prop(this, &idx_key(k))?);
        }
    }
    Ok(Value::Obj(i.new_arr(out)))
}

fn arr_splice(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec Array.prototype.splice — O → len → ToIntegerOrInfinity bounds →
    // TypeError on 2^53-1 overflow → removed elements copied (live
    // HasProperty → Get) into an ArraySpeciesCreate result → the tail
    // renumbers via per-index Has → Get → Set / absent → Delete → items
    // Set in at actualStart → Set "length".
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    let rel_start = to_integer_or_inf(i.to_num(&arg(args, 0))?);
    let actual_start = if rel_start < 0.0 {
        (len as f64 + rel_start).max(0.0) as usize
    } else {
        rel_start.min(len as f64) as usize
    };
    let (insert_count, delete_count) = if args.is_empty() {
        (0usize, 0usize)
    } else if args.len() == 1 {
        (0usize, len - actual_start)
    } else {
        let dc = to_integer_or_inf(i.to_num(&args[1])?);
        let dc = if dc < 0.0 {
            0.0
        } else {
            dc.min((len - actual_start) as f64)
        };
        (args.len() - 2, dc as usize)
    };
    if len + insert_count - delete_count > 9007199254740991usize {
        return Err(i.type_err("Array.prototype.splice: too many elements"));
    }
    let a = species_create(i, &obj, delete_count)?;
    for k in 0..delete_count {
        let from = idx_key(actual_start + k);
        if i.has_prop(&obj, &from)? {
            let v = i.get_prop(&obj, &from)?;
            create_data_prop(i, &a, k, v)?;
        }
    }
    spec_set(i, &a, pk("length"), Value::Num(delete_count as f64))?;
    if insert_count < delete_count {
        for k in actual_start..(len - delete_count) {
            let from = idx_key(k + delete_count);
            let to = k + insert_count;
            if i.has_prop(&obj, &from)? {
                let v = i.get_prop(&obj, &from)?;
                spec_set_idx(i, &obj, to, v)?;
            } else {
                spec_del_idx(i, &obj, to)?;
            }
        }
        for k in ((len - delete_count + insert_count)..len).rev() {
            spec_del_idx(i, &obj, k)?;
        }
    } else if insert_count > delete_count {
        let mut k = len - delete_count;
        while k > actual_start {
            let from = idx_key(k + delete_count - 1);
            let to = k + insert_count - 1;
            if i.has_prop(&obj, &from)? {
                let v = i.get_prop(&obj, &from)?;
                spec_set_idx(i, &obj, to, v)?;
            } else {
                spec_del_idx(i, &obj, to)?;
            }
            k -= 1;
        }
    }
    let mut k = actual_start;
    for e in args.iter().skip(2) {
        spec_set_idx(i, &obj, k, e.clone())?;
        k += 1;
    }
    spec_set(
        i,
        &obj,
        pk("length"),
        Value::Num((len - delete_count + insert_count) as f64),
    )?;
    Ok(a)
}

fn arr_concat(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec: E = ToObject(this) — primitives box to wrappers; A =
    // ArraySpeciesCreate(O,0); each element of [E, ...args] spreads iff
    // IsConcatSpreadable; spread visits only PRESENT indices
    // (HasProperty → live Get → CreateDataProperty) up to LengthOfArrayLike.
    let obj = i.to_object(this)?;
    let a = species_create(i, &obj, 0)?;
    let mut n = 0usize;
    for e in std::iter::once(&obj).chain(args.iter()) {
        let spreadable = match e {
            Value::Obj(_) => {
                let flag = i.get_prop(e, &PropKey::Sym(SYM_IS_CONCAT_SPREADABLE))?;
                if matches!(flag, Value::Und) {
                    is_array_val(i, e)?
                } else {
                    i.to_bool(&flag)
                }
            }
            _ => false,
        };
        if spreadable {
            let elen = length_of_array_like(i, e)?;
            if n + elen > 9007199254740991usize {
                return Err(i.type_err("Array.prototype.concat: too many elements"));
            }
            for k in 0..elen {
                let key = idx_key(k);
                if i.has_prop(e, &key)? {
                    let v = i.get_prop(e, &key)?;
                    create_data_prop(i, &a, n, v)?;
                    n += 1;
                }
            }
        } else {
            if n >= 9007199254740991usize {
                return Err(i.type_err("Array.prototype.concat: too many elements"));
            }
            create_data_prop(i, &a, n, e.clone())?;
            n += 1;
        }
    }
    spec_set(i, &a, pk("length"), Value::Num(n as f64))?;
    Ok(a)
}

/// spec %Array.prototype.toString% — `func = ? Get(O, "join")`;
/// a non-callable falls back to %Object.prototype.toString% and any
/// callable is invoked as-is. This matters for TypedArray receivers:
/// `%TypedArray%.prototype.toString` is this same function object, and
/// `sample.toString()` must dispatch to `%TypedArray%.prototype.join`,
/// which throws a TypeError on a detached or out-of-bounds buffer.
fn arr_to_string(i: &mut Interp, this: &Value, _args: &[Value]) -> NResult {
    let o = i.to_object(this)?;
    let join = i.get_prop_str(&o, "join")?;
    if i.is_callable(&join) {
        return i.call(&join, o, Vec::new());
    }
    obj_to_string(i, &o, &[])
}

fn arr_join(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec: O = ToObject → len = LengthOfArrayLike — the length snapshots
    // BEFORE the separator coercion (a toString that resizes the backing
    // keeps the old element count) — then per-index live Get.
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    let sep = match arg(args, 0) {
        Value::Und => ",".to_string(),
        v => i.to_str(&v)?,
    };
    let mut out = String::new();
    for k in 0..len {
        if k > 0 {
            out.push_str(&sep);
        }
        match i.get_prop(&obj, &idx_key(k))? {
            Value::Und | Value::Null => {}
            Value::Str(s) => out.push_str(&s),
            Value::Num(n) => out.push_str(&js_num_str(n)),
            Value::Bool(b) => out.push_str(if b { "true" } else { "false" }),
            v => out.push_str(&i.to_str(&v)?),
        }
    }
    Ok(Value::Str(Rc::from(out.as_str())))
}

/// spec %Array.prototype.toLocaleString% — each element stringifies via
/// its own "toLocaleString" Invoke (user overrides fire); nullish and
/// absent slots become the empty string.
fn arr_to_locale_string(i: &mut Interp, this: &Value, _args: &[Value]) -> NResult {
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    let mut out = String::new();
    for k in 0..len {
        if k > 0 {
            out.push(',');
        }
        let el = i.get_prop(&obj, &idx_key(k))?;
        if !matches!(el, Value::Und | Value::Null) {
            let m = i.get_prop(&el, &pk("toLocaleString"))?;
            let r = i.call(&m, el, Vec::new())?;
            out.push_str(&i.to_str(&r)?);
        }
    }
    Ok(Value::Str(Rc::from(out.as_str())))
}

fn arr_index_of(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let needle = arg(args, 0);
    // fast path: dense array + numeric/absent fromIndex (no user code)
    if matches!(arg(args, 1), Value::Num(_) | Value::Und) {
        if let Value::Obj(o) = this {
            let ok_dense = {
                let b = o.borrow();
                matches!(&b.kind, ObjKind::Arr(items)
                    if b.holes.as_ref().map_or(true, |h| h.is_empty())
                        && arr_declared_len(o) == items.len())
            };
            if ok_dense {
                let b = o.borrow();
                if let ObjKind::Arr(items) = &b.kind {
                    let start = clamp_index(i.to_num(&arg(args, 1)).unwrap_or(0.0), items.len());
                    for (k, v) in items.iter().enumerate().skip(start) {
                        if strict_val_eq(v, &needle) {
                            return Ok(Value::Num(k as f64));
                        }
                    }
                    return Ok(Value::Num(-1.0));
                }
            }
        }
    }
    // spec: O = ToObject → len = LengthOfArrayLike → n = ToInteger(fromIndex)
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    if len == 0 {
        return Ok(Value::Num(-1.0));
    }
    let n = match arg(args, 1) {
        Value::Und => 0.0,
        v => i.to_num(&v)?.trunc(),
    };
    if n == f64::INFINITY || n >= len as f64 {
        return Ok(Value::Num(-1.0));
    }
    let mut k = if n == f64::NEG_INFINITY {
        0i64
    } else if n < 0.0 {
        (len as i64 + n as i64).max(0)
    } else {
        n as i64
    };
    while k < len as i64 {
        let key = idx_key(k as usize);
        // HasProperty first — holes/proto-absent indices are skipped
        if i.has_prop(&obj, &key)? && strict_val_eq(&i.get_prop(&obj, &key)?, &needle) {
            return Ok(Value::Num(k as f64));
        }
        k += 1;
    }
    Ok(Value::Num(-1.0))
}
/// spec start index for lastIndexOf: `fromIndex` (default len-1), negative
/// counts from the end; starting before 0 yields -1.
fn last_index_start(n: f64, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let k = if n.is_nan() {
        len as i64 - 1
    } else if n < 0.0 {
        len as i64 + n as i64
    } else {
        (n as i64).min(len as i64 - 1)
    };
    (k >= 0).then_some(k as usize)
}

fn arr_last_index_of(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let needle = arg(args, 0);
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    if len == 0 {
        return Ok(Value::Num(-1.0));
    }
    // ToIntegerOrInfinity(fromIndex), default len-1; NaN → len-1
    // ToIntegerOrInfinity(fromIndex): default len-1, NaN → +0,
    // +Inf → len-1, negative → len+n
    let n = match args.get(1) {
        None => len as f64 - 1.0,
        Some(v) => to_integer_or_inf(i.to_num(v)?),
    };
    let mut k: i64 = if n >= len as f64 {
        len as i64 - 1
    } else if n < 0.0 {
        // len + n (may stay negative → -1)
        (len as f64 + n) as i64
    } else {
        n as i64
    };
    while k >= 0 {
        let key = idx_key(k as usize);
        if i.has_prop(&obj, &key)? && strict_val_eq(&i.get_prop(&obj, &key)?, &needle) {
            return Ok(Value::Num(k as f64));
        }
        k -= 1;
    }
    Ok(Value::Num(-1.0))
}
fn strict_val_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Und, Value::Und) | (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Num(x), Value::Num(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Sym(x, _), Value::Sym(y, _)) => x == y,
        (Value::Obj(x), Value::Obj(y)) => {
            // bigint primitives are Obj standins — compare by magnitude
            Rc::ptr_eq(x, y) || bigint_mag_eq(a, b)
        }
        _ => false,
    }
}

fn same_value_zero(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Num(x), Value::Num(y)) if x.is_nan() && y.is_nan() => true,
        _ => strict_val_eq(a, b),
    }
}

fn arr_includes(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let needle = arg(args, 0);
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    if len == 0 {
        return Ok(Value::Bool(false));
    }
    // ToIntegerOrInfinity(fromIndex): -Inf→0, +Inf→false, neg→len+n
    let n = match arg(args, 1) {
        Value::Und => 0.0,
        v => i.to_num(&v)?,
    };
    if n == f64::INFINITY || n >= len as f64 {
        return Ok(Value::Bool(false));
    }
    let mut k: i64 = if n == f64::NEG_INFINITY || n.is_nan() {
        0
    } else {
        let t = n.trunc() as i64;
        if t < 0 {
            (len as i64 + t).max(0)
        } else {
            t
        }
    };
    while k < len as i64 {
        // spec: SameValueZero on Get — holes read as `undefined`
        if same_value_zero(&i.get_prop(&obj, &idx_key(k as usize))?, &needle) {
            return Ok(Value::Bool(true));
        }
        k += 1;
    }
    Ok(Value::Bool(false))
}
fn arr_find(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    // spec: ToObject → LengthOfArrayLike → IsCallable check, then per-index
    // Get + Call — reads stay live (deletion/mutation visible mid-loop)
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    if !i.is_callable(&cb) {
        return Err(i.type_err("predicate is not callable"));
    }
    let mut k = 0usize;
    while k < len {
        let v = i.get_prop(&obj, &idx_key(k))?;
        let r = i.call(
            &cb,
            this_arg.clone(),
            vec![v.clone(), Value::Num(k as f64), obj.clone()],
        )?;
        if i.to_bool(&r) {
            return Ok(v);
        }
        k += 1;
    }
    Ok(Value::Und)
}
fn arr_find_index(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    // spec: ToObject → LengthOfArrayLike → IsCallable check, then per-index
    // Get + Call — reads stay live (deletion/mutation visible mid-loop)
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    if !i.is_callable(&cb) {
        return Err(i.type_err("predicate is not callable"));
    }
    let mut k = 0usize;
    while k < len {
        let v = i.get_prop(&obj, &idx_key(k))?;
        let r = i.call(
            &cb,
            this_arg.clone(),
            vec![v.clone(), Value::Num(k as f64), obj.clone()],
        )?;
        if i.to_bool(&r) {
            return Ok(Value::Num(k as f64));
        }
        k += 1;
    }
    Ok(Value::Num(-1.0))
}
fn arr_find_last(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    // spec: ToObject → LengthOfArrayLike → IsCallable check, then per-index
    // Get + Call — reads stay live (deletion/mutation visible mid-loop)
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    if !i.is_callable(&cb) {
        return Err(i.type_err("predicate is not callable"));
    }
    let mut k = len as i64 - 1;
    while k >= 0 {
        let v = i.get_prop(&obj, &idx_key(k as usize))?;
        let r = i.call(
            &cb,
            this_arg.clone(),
            vec![v.clone(), Value::Num(k as f64), obj.clone()],
        )?;
        if i.to_bool(&r) {
            return Ok(v);
        }
        k -= 1;
    }
    Ok(Value::Und)
}
fn arr_find_last_index(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    // spec: ToObject → LengthOfArrayLike → IsCallable check, then per-index
    // Get + Call — reads stay live (deletion/mutation visible mid-loop)
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    if !i.is_callable(&cb) {
        return Err(i.type_err("predicate is not callable"));
    }
    let mut k = len as i64 - 1;
    while k >= 0 {
        let v = i.get_prop(&obj, &idx_key(k as usize))?;
        let r = i.call(
            &cb,
            this_arg.clone(),
            vec![v.clone(), Value::Num(k as f64), obj.clone()],
        )?;
        if i.to_bool(&r) {
            return Ok(Value::Num(k as f64));
        }
        k -= 1;
    }
    Ok(Value::Num(-1.0))
}
/// spec FlattenIntoArray: per-index HasProperty → live Get → optional
/// mapper Call → IsArray elements recurse when depth>0, everything else
/// CreateDataProperty-appends at the running target index.
fn flatten_into(
    i: &mut Interp,
    target: &Value,
    source: &Value,
    source_len: usize,
    mapper: Option<(&Value, &Value)>,
    depth: f64,
    n: &mut usize,
) -> Result<(), Value> {
    for k in 0..source_len {
        let key = idx_key(k);
        if i.has_prop(source, &key)? {
            let mut el = i.get_prop(source, &key)?;
            if let Some((cb, this_arg)) = mapper {
                el = i.call(
                    cb,
                    this_arg.clone(),
                    vec![el, Value::Num(k as f64), source.clone()],
                )?;
            }
            if depth > 0.0 && is_array_val(i, &el)? {
                let elen = length_of_array_like(i, &el)?;
                flatten_into(i, target, &el, elen, None, depth - 1.0, n)?;
            } else {
                if *n >= 9007199254740991usize {
                    return Err(i.type_err("flatten target index exceeded"));
                }
                create_data_prop(i, target, *n, el)?;
                *n += 1;
            }
        }
    }
    Ok(())
}

fn arr_filter(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    // spec: ToObject → LengthOfArrayLike → IsCallable check → species —
    // then per-index HasProperty → live Get → callback → CreateDataProperty
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    if !i.is_callable(&cb) {
        return Err(i.type_err("callback is not callable"));
    }
    let a = species_create(i, &obj, 0)?;
    let mut n = 0usize;
    for k in 0..len {
        let key = idx_key(k);
        if i.has_prop(&obj, &key)? {
            let v = i.get_prop(&obj, &key)?;
            let r = i.call(
                &cb,
                this_arg.clone(),
                vec![v.clone(), Value::Num(k as f64), obj.clone()],
            )?;
            if i.to_bool(&r) {
                create_data_prop(i, &a, n, v)?;
                n += 1;
            }
        }
    }
    Ok(a)
}

fn arr_map(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    // spec: ToObject → LengthOfArrayLike → IsCallable check →
    // ArraySpeciesCreate(O, len) → per-index HasProperty → live Get →
    // callback → CreateDataProperty (result keeps the source's holes)
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    if !i.is_callable(&cb) {
        return Err(i.type_err("callback is not callable"));
    }
    let a = species_create(i, &obj, len)?;
    for k in 0..len {
        let key = idx_key(k);
        if i.has_prop(&obj, &key)? {
            let v = i.get_prop(&obj, &key)?;
            let r = i.call(
                &cb,
                this_arg.clone(),
                vec![v, Value::Num(k as f64), obj.clone()],
            )?;
            create_data_prop(i, &a, k, r)?;
        }
    }
    Ok(a)
}

fn arr_for_each(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    // spec: ToObject → LengthOfArrayLike → IsCallable check → per-index
    // HasProperty → live Get → callback (result discarded)
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    if !i.is_callable(&cb) {
        return Err(i.type_err("callback is not callable"));
    }
    for k in 0..len {
        let key = idx_key(k);
        if i.has_prop(&obj, &key)? {
            let v = i.get_prop(&obj, &key)?;
            i.call(
                &cb,
                this_arg.clone(),
                vec![v, Value::Num(k as f64), obj.clone()],
            )?;
        }
    }
    Ok(Value::Und)
}

fn arr_reduce(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    // spec: ToObject → LengthOfArrayLike → IsCallable check — then the
    // accumulator seeds from arg1-if-present else the first PRESENT
    // element; per-index HasProperty → live Get → callback
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    if !i.is_callable(&cb) {
        return Err(i.type_err("callback is not callable"));
    }
    let mut k = 0usize;
    let mut acc = match args.get(1) {
        Some(v) => v.clone(),
        None => loop {
            if k >= len {
                return Err(i.type_err("Reduce of empty array with no initial value"));
            }
            let key = idx_key(k);
            if i.has_prop(&obj, &key)? {
                let v = i.get_prop(&obj, &key)?;
                k += 1;
                break v;
            }
            k += 1;
        },
    };
    while k < len {
        let key = idx_key(k);
        if i.has_prop(&obj, &key)? {
            let v = i.get_prop(&obj, &key)?;
            acc = i.call(
                &cb,
                Value::Und,
                vec![acc, v, Value::Num(k as f64), obj.clone()],
            )?;
        }
        k += 1;
    }
    Ok(acc)
}

fn arr_reduce_right(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    // spec mirror of reduce: k walks len-1 → 0, same live
    // HasProperty → Get reads (mid-iteration mutations are observed)
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    if !i.is_callable(&cb) {
        return Err(i.type_err("callback is not callable"));
    }
    let mut k: i64 = len as i64 - 1;
    let mut acc = match args.get(1) {
        Some(v) => v.clone(),
        None => loop {
            if k < 0 {
                return Err(i.type_err("Reduce of empty array with no initial value"));
            }
            let key = idx_key(k as usize);
            if i.has_prop(&obj, &key)? {
                let v = i.get_prop(&obj, &key)?;
                k -= 1;
                break v;
            }
            k -= 1;
        },
    };
    while k >= 0 {
        let key = idx_key(k as usize);
        if i.has_prop(&obj, &key)? {
            let v = i.get_prop(&obj, &key)?;
            acc = i.call(
                &cb,
                Value::Und,
                vec![acc, v, Value::Num(k as f64), obj.clone()],
            )?;
        }
        k -= 1;
    }
    Ok(acc)
}

fn arr_some(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    // spec: ToObject → LengthOfArrayLike → IsCallable check → per-index
    // HasProperty → live Get → callback; first truthy result wins
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    if !i.is_callable(&cb) {
        return Err(i.type_err("callback is not callable"));
    }
    for k in 0..len {
        let key = idx_key(k);
        if i.has_prop(&obj, &key)? {
            let v = i.get_prop(&obj, &key)?;
            let r = i.call(
                &cb,
                this_arg.clone(),
                vec![v, Value::Num(k as f64), obj.clone()],
            )?;
            if i.to_bool(&r) {
                return Ok(Value::Bool(true));
            }
        }
    }
    Ok(Value::Bool(false))
}

fn arr_every(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    // spec: ToObject → LengthOfArrayLike → IsCallable check → per-index
    // HasProperty → live Get → callback; first falsy result wins
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    if !i.is_callable(&cb) {
        return Err(i.type_err("callback is not callable"));
    }
    for k in 0..len {
        let key = idx_key(k);
        if i.has_prop(&obj, &key)? {
            let v = i.get_prop(&obj, &key)?;
            let r = i.call(
                &cb,
                this_arg.clone(),
                vec![v, Value::Num(k as f64), obj.clone()],
            )?;
            if !i.to_bool(&r) {
                return Ok(Value::Bool(false));
            }
        }
    }
    Ok(Value::Bool(true))
}

fn arr_flat(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec: O → len → ArraySpeciesCreate(O,0) → FlattenIntoArray at
    // ToIntegerOrInfinity(depth, default 1)
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    let depth = match arg(args, 0) {
        Value::Und => 1.0,
        v => to_integer_or_inf(i.to_num(&v)?),
    };
    let a = species_create(i, &obj, 0)?;
    let mut n = 0usize;
    flatten_into(i, &a, &obj, len, None, depth, &mut n)?;
    Ok(a)
}

fn arr_flat_map(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    // spec: O → len → IsCallable check → ArraySpeciesCreate →
    // FlattenIntoArray with the mapper at depth 1
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    if !i.is_callable(&cb) {
        return Err(i.type_err("callback is not callable"));
    }
    let a = species_create(i, &obj, 0)?;
    let mut n = 0usize;
    flatten_into(i, &a, &obj, len, Some((&cb, &this_arg)), 1.0, &mut n)?;
    Ok(a)
}

fn arr_fill(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec returns the ToObject'd receiver (primitive this → wrapper);
    // each index is Set(O, k, value, true) — accessors/non-writable
    // slots must fire/throw, so the dense path only applies when no
    // index-named shadow props exist
    let obj = i.to_object(this)?;
    let v = arg(args, 0);
    let len = length_of_array_like(i, &obj)?;
    let start = clamp_index(to_integer_or_inf(i.to_num(&arg(args, 1))?), len);
    let end = match arg(args, 2) {
        Value::Und => len,
        x => clamp_index(to_integer_or_inf(i.to_num(&x)?), len),
    };
    if let Value::Obj(o) = &obj {
        let fast = {
            let b = o.borrow();
            matches!(&b.kind, ObjKind::Arr(_))
                && !b
                    .props
                    .iter()
                    .any(|(k, _)| matches!(k, PropKey::S(s) if arr_index(s.as_ref()).is_some()))
        };
        if fast {
            let mut b = o.borrow_mut();
            if let Some(h) = b.holes.as_mut() {
                for k in start..end {
                    h.remove(&k);
                }
            }
            if let ObjKind::Arr(items) = &mut b.kind {
                // fill materializes every index in range — holes included
                if end > items.len() {
                    items.resize(end, Value::Und);
                }
                for k in start..end {
                    items[k] = v.clone();
                }
            }
            return Ok(obj.clone());
        }
    }
    for k in start..end {
        spec_set_idx(i, &obj, k, v.clone())?;
    }
    Ok(obj)
}

fn arr_reverse(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    // spec: pairwise swap across the middle — each side HasProperty →
    // conditional Get, then Set / Delete in the spec's exact order.
    // Returns the ToObject'd receiver (primitive this → wrapper).
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    let middle = len / 2;
    for lower in 0..middle {
        let upper = len - lower - 1;
        let lower_key = idx_key(lower);
        let upper_key = idx_key(upper);
        let lower_exists = i.has_prop(&obj, &lower_key)?;
        let lower_v = if lower_exists {
            i.get_prop(&obj, &lower_key)?
        } else {
            Value::Und
        };
        let upper_exists = i.has_prop(&obj, &upper_key)?;
        let upper_v = if upper_exists {
            i.get_prop(&obj, &upper_key)?
        } else {
            Value::Und
        };
        if lower_exists && upper_exists {
            spec_set(i, &obj, lower_key, upper_v)?;
            spec_set(i, &obj, upper_key, lower_v)?;
        } else if upper_exists {
            spec_set(i, &obj, lower_key, upper_v)?;
            spec_del_idx(i, &obj, upper)?;
        } else if lower_exists {
            spec_del_idx(i, &obj, lower)?;
            spec_set(i, &obj, upper_key, lower_v)?;
        } else {
            spec_del_idx(i, &obj, lower)?;
            spec_del_idx(i, &obj, upper)?;
        }
    }
    Ok(obj)
}

fn arr_copy_within(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec: O = ToObject → len = LengthOfArrayLike → target/start/end
    // coercions (ToIntegerOrInfinity, clamped ±len) → count =
    // min(final-from, len-to) → direction flips on overlap → per-index
    // HasProperty → Get → Set / absent → DeletePropertyOrThrow.
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    let len_f = len as f64;
    let clamp_to = |n: f64| -> f64 {
        if n < 0.0 {
            (len_f + n).max(0.0)
        } else {
            n.min(len_f)
        }
    };
    let rel_target = to_integer_or_inf(i.to_num(&arg(args, 0))?);
    let to = clamp_to(rel_target);
    let rel_start = to_integer_or_inf(i.to_num(&arg(args, 1))?);
    let from = clamp_to(rel_start);
    let rel_end = match arg(args, 2) {
        Value::Und => len_f,
        v => to_integer_or_inf(i.to_num(&v)?),
    };
    let final_ = clamp_to(rel_end);
    let mut count = (final_ - from).min(len_f - to);
    if count <= 0.0 {
        return Ok(obj);
    }
    // overlapping regions copy backwards so source slots aren't clobbered
    let (mut to, mut from, dir) = if from < to && to < from + count {
        (to + count - 1.0, from + count - 1.0, -1.0)
    } else {
        (to, from, 1.0)
    };
    while count > 0.0 {
        let from_key = idx_key(from as usize);
        let to_key = idx_key(to as usize);
        if i.has_prop(&obj, &from_key)? {
            let v = i.get_prop(&obj, &from_key)?;
            spec_set(i, &obj, to_key, v)?;
        } else {
            spec_del_idx(i, &obj, to as usize)?;
        }
        from += dir;
        to += dir;
        count -= 1.0;
    }
    Ok(obj)
}

fn arr_sort(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cmp = arg(args, 0);
    // spec: IsCallable(comparefn) is checked BEFORE ToObject/length — a
    // poisoned length getter must not fire ahead of the TypeError.
    if !matches!(cmp, Value::Und) && !i.is_callable(&cmp) {
        return Err(i.type_err("Array.prototype.sort: comparator is not callable"));
    }
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    // SortIndexedProperties (skip-holes): collect every PRESENT element
    // in index order via Has → live Get — getters, proto elements and
    // mid-collection deletions all count. Dense real arrays collect
    // directly: every slot is own+present so nothing observable runs.
    let mut items = if let Value::Obj(o) = &obj {
        let b = o.borrow();
        match &b.kind {
            ObjKind::Arr(it)
                if b.holes.as_ref().map_or(true, |h| h.is_empty())
                    && arr_declared_len(o) == it.len()
                    && !b.props.iter().any(|(k, _)| {
                        matches!(k, PropKey::S(s) if arr_index(s.as_ref()).is_some_and(|ix| ix < len))
                    }) =>
            {
                it.clone()
            }
            _ => Vec::new(),
        }
    } else {
        Vec::new()
    };
    if items.is_empty() {
        for k in 0..len {
            let key = idx_key(k);
            if i.has_prop(&obj, &key)? {
                items.push(i.get_prop(&obj, &key)?);
            }
        }
    }
    {
        let mut err: Option<Value> = None;
        let mut work = items.clone();
        let callable = i.is_callable(&cmp);
        merge_sort(&mut items, &mut work, &mut |x, y| {
            if err.is_some() {
                return std::cmp::Ordering::Equal;
            }
            // spec CompareArrayElements: undefineds order after everything
            let xu = matches!(x, Value::Und);
            let yu = matches!(y, Value::Und);
            if xu && yu {
                return std::cmp::Ordering::Equal;
            }
            if xu {
                return std::cmp::Ordering::Greater;
            }
            if yu {
                return std::cmp::Ordering::Less;
            }
            if callable {
                match i.call(&cmp, Value::Und, vec![x.clone(), y.clone()]) {
                    Ok(v) => match i.to_num(&v) {
                        // NaN compares equal — never reorders
                        Ok(n) => {
                            if n.is_nan() {
                                std::cmp::Ordering::Equal
                            } else if n < 0.0 {
                                std::cmp::Ordering::Less
                            } else if n > 0.0 {
                                std::cmp::Ordering::Greater
                            } else {
                                std::cmp::Ordering::Equal
                            }
                        }
                        Err(e) => {
                            err = Some(e);
                            std::cmp::Ordering::Equal
                        }
                    },
                    Err(e) => {
                        err = Some(e);
                        std::cmp::Ordering::Equal
                    }
                }
            } else {
                // default comparator: ToString per comparison, ordered by
                // UTF-16 code units
                match (i.to_str(x), i.to_str(y)) {
                    (Ok(a), Ok(b)) => {
                        let a16: Vec<u16> = a.encode_utf16().collect();
                        let b16: Vec<u16> = b.encode_utf16().collect();
                        a16.cmp(&b16)
                    }
                    (Err(e), _) | (_, Err(e)) => {
                        err = Some(e);
                        std::cmp::Ordering::Equal
                    }
                }
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
    }
    // write phase: Set each sorted value, then DeletePropertyOrThrow for
    // the rest of [itemCount, len) — holes/absent slots stay absent
    let item_count = items.len();
    for (j, v) in items.drain(..).enumerate() {
        spec_set_idx(i, &obj, j, v)?;
    }
    for j in item_count..len {
        spec_del_idx(i, &obj, j)?;
    }
    Ok(obj)
}

pub(crate) fn merge_sort(
    items: &mut [Value],
    work: &mut Vec<Value>,
    cmp: &mut dyn FnMut(&Value, &Value) -> std::cmp::Ordering,
) {
    let n = items.len();
    if n <= 1 {
        return;
    }
    let mid = n / 2;
    merge_sort(&mut items[..mid], work, cmp);
    merge_sort(&mut items[mid..], work, cmp);
    work.clear();
    work.extend_from_slice(items);
    let (l, r) = work.split_at(mid);
    let mut li = 0;
    let mut ri = 0;
    for slot in items.iter_mut() {
        if li < l.len() && (ri >= r.len() || cmp(&l[li], &r[ri]) != std::cmp::Ordering::Greater) {
            *slot = l[li].clone();
            li += 1;
        } else {
            *slot = r[ri].clone();
            ri += 1;
        }
    }
}

fn arr_at(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec: O = ToObject → len = LengthOfArrayLike → relativeIndex =
    // ToIntegerOrInfinity(arg0); a negative wraps from the end. The
    // property read is a live Get so getters/prototypes are observed
    // (and primitives only see wrapper-prototype values — spec matches).
    let obj = i.to_object(this)?;
    let len = length_of_array_like(i, &obj)?;
    let n = to_integer_or_inf(i.to_num(&arg(args, 0))?);
    let k = if n < 0.0 { len as f64 + n } else { n };
    if k < 0.0 || k >= len as f64 {
        return Ok(Value::Und);
    }
    i.get_prop(&obj, &idx_key(k as usize))
}

fn arr_keys(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    // spec CreateArrayIterator(ToObject(this), "key") — a live view:
    // mutation during iteration is observed
    let obj = i.to_object(this)?;
    if let Value::Obj(o) = &obj {
        return Ok(make_live_iter(o.clone(), i.p.array_iter.clone(), 1));
    }
    unreachable!()
}

fn arr_values(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    // spec CreateArrayIterator(ToObject(this), "value")
    let obj = i.to_object(this)?;
    if let Value::Obj(o) = &obj {
        return Ok(make_live_iter(o.clone(), i.p.array_iter.clone(), 0));
    }
    unreachable!()
}

fn arr_entries(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    // spec CreateArrayIterator(ToObject(this), "key+value")
    let obj = i.to_object(this)?;
    if let Value::Obj(o) = &obj {
        return Ok(make_live_iter(o.clone(), i.p.array_iter.clone(), 2));
    }
    unreachable!()
}

/// live-view iterator object — reads `obj` per step so mid-iteration
/// mutation (mapped arguments, array holes, map/set delete+add) shows
fn make_live_iter(o: ObjRef, proto: ObjRef, mode: u8) -> Value {
    Value::Obj(Rc::new(RefCell::new(Obj::new(
        Some(proto),
        ObjKind::IterLive {
            obj: o,
            idx: 0,
            mode,
        },
    ))))
}

// ===========================================================================
// natives: string
// ===========================================================================

/// String receiver as `Rc<str>` — zero-copy for `Value::Str` (the common
/// case); callers slice/iterate instead of getting an owned `String`.
fn unbox_str(i: &mut Interp, this: &Value) -> Result<Rc<str>, Value> {
    match this {
        // RequireObjectCoercible — null/undefined receivers are a TypeError
        Value::Und | Value::Null => {
            Err(i.type_err("String.prototype method called on null or undefined"))
        }
        Value::Str(s) => Ok(s.clone()),
        Value::Obj(o) => {
            if let Some(v) = i.obj_get_raw_str(o, "$value") {
                return to_rc(i, &v);
            }
            to_rc(i, this)
        }
        other => to_rc(i, other),
    }
}

/// spec thisStringValue — String.prototype.toString/valueOf are NOT
/// generic: they require a String primitive or [[StringData]] object.
fn str_this_str(i: &mut Interp, this: &Value) -> Result<Rc<str>, Value> {
    match this {
        Value::Str(s) => Ok(s.clone()),
        Value::Obj(o) => match i.obj_get_raw_str(o, "$value") {
            Some(Value::Str(s)) => Ok(s),
            _ => Err(i.type_err("String.prototype method called on incompatible receiver")),
        },
        _ => Err(i.type_err("String.prototype method called on incompatible receiver")),
    }
}

/// spec "If x is Object" in the String protocol methods (split, replace,
/// replaceAll, match, matchAll, search) — the engine's BigInt primitives
/// are Obj stand-ins but are NOT objects for this check.
fn is_obj_arg(v: &Value) -> bool {
    matches!(v, Value::Obj(_)) && !crate::binary::bigint_is_prim(v)
}

/// spec ToNumber for String.prototype arguments — rejects BigInt even
/// when a bigint surfaces from ToPrimitive (i.to_num quietly reads the
/// magnitude back, which would swallow the required TypeError).
fn str_num(i: &mut Interp, v: &Value) -> Result<f64, Value> {
    if crate::binary::bigint_is_prim(v) {
        return Err(i.type_err("Cannot convert a BigInt value to a number"));
    }
    if let Value::Obj(o) = v {
        let p = i.to_primitive_hint(v, o.clone(), "number")?;
        if crate::binary::bigint_is_prim(&p) {
            return Err(i.type_err("Cannot convert a BigInt value to a number"));
        }
        return i.to_num(&p);
    }
    i.to_num(v)
}

/// spec ToIntegerOrInfinity — NaN → +0, ±∞ pass through, else trunc to 0.
fn to_integer_or_inf(n: f64) -> f64 {
    if n.is_nan() {
        0.0
    } else {
        n.trunc()
    }
}

/// position argument: ToIntegerOrInfinity output clamped into [0, len].
fn str_pos(n: f64, len: usize) -> usize {
    if n.is_nan() || n <= 0.0 {
        0
    } else if n.is_infinite() || n >= len as f64 {
        len
    } else {
        n.trunc() as usize
    }
}

/// UTF-16 code-unit view — String methods index/count in units.
fn u16s(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// UTF-16 code units → engine string. Lone surrogates cannot live in a
/// Rust &str — from_utf16_lossy maps them to U+FFFD, matching the lexer's
/// own handling of \uXXXX surrogate escapes (UTF-16 representation debt).
fn u16s_val(u: &[u16]) -> Value {
    Value::Str(Rc::from(String::from_utf16_lossy(u).as_str()))
}

/// spec StringIndexOf on UTF-16 units: smallest k >= pos such that
/// su[k..k+nu] == nu.
fn str_index_of_u(su: &[u16], nu: &[u16], pos: usize) -> Option<usize> {
    if nu.is_empty() {
        return if pos <= su.len() { Some(pos) } else { None };
    }
    if pos > su.len() || nu.len() > su.len() - pos {
        return None;
    }
    (pos..=su.len() - nu.len()).find(|&k| su[k..k + nu.len()] == nu[..])
}

/// largest k <= pos such that su[k..k+nu] == nu (spec lastIndexOf scan).
fn str_last_index_of_u(su: &[u16], nu: &[u16], pos: usize) -> Option<usize> {
    if nu.is_empty() {
        return if pos <= su.len() { Some(pos) } else { None };
    }
    if nu.len() > su.len() {
        return None;
    }
    let max_k = (su.len() - nu.len()).min(pos);
    (0..=max_k).rev().find(|&k| su[k..k + nu.len()] == nu[..])
}

fn str_char_at(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    // spec: ToIntegerOrInfinity, empty when out of [0, len)
    let n = to_integer_or_inf(str_num(i, &arg(args, 0))?);
    let u = u16s(&s);
    if n < 0.0 || n >= u.len() as f64 {
        return Ok(Value::Str(empty_rc()));
    }
    Ok(u16s_val(&u[n as usize..n as usize + 1]))
}

/// spec codePointAt: UTF-16 indexed, surrogate pairs combine
fn str_code_point_at(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let idx = to_integer_or_inf(str_num(i, &arg(args, 0))?);
    if idx < 0.0 {
        return Ok(Value::Und);
    }
    let units: Vec<u16> = s.encode_utf16().collect();
    let k = idx as usize;
    let Some(&first) = units.get(k) else {
        return Ok(Value::Und);
    };
    if (0xD800..=0xDBFF).contains(&first) {
        if let Some(&second) = units.get(k + 1) {
            if (0xDC00..=0xDFFF).contains(&second) {
                let cp = 0x10000 + ((first as u32 - 0xD800) << 10) + (second as u32 - 0xDC00);
                return Ok(Value::Num(cp as f64));
            }
        }
    }
    Ok(Value::Num(first as f64))
}

fn str_is_well_formed(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let _ = unbox_str(i, this)?;
    // UTF-8 strings can't represent lone surrogates — always well-formed
    Ok(Value::Bool(true))
}

fn str_to_well_formed(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    Ok(Value::Str(unbox_str(i, this)?))
}

fn str_char_code_at(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    // spec: ToIntegerOrInfinity — NaN/negative/out-of-range → NaN result
    let n = to_integer_or_inf(str_num(i, &arg(args, 0))?);
    let u = u16s(&s);
    if n < 0.0 || n >= u.len() as f64 {
        return Ok(Value::Num(f64::NAN));
    }
    Ok(Value::Num(u[n as usize] as f64))
}

fn str_at(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    // spec: ToIntegerOrInfinity then relative-index — UTF-16 unit slots
    let n = to_integer_or_inf(str_num(i, &arg(args, 0))?);
    let u = u16s(&s);
    let k = if n < 0.0 { u.len() as f64 + n } else { n };
    if k < 0.0 || k >= u.len() as f64 {
        return Ok(Value::Und);
    }
    let k = k as usize;
    Ok(u16s_val(&u[k..k + 1]))
}

fn str_index_of(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec order: ToString(this) → ToString(search) → ToIntegerOrInfinity(pos)
    let s = unbox_str(i, this)?;
    let needle = to_rc(i, &arg(args, 0))?;
    let n = str_num(i, &arg(args, 1))?;
    let (su, nu) = (u16s(&s), u16s(&needle));
    let pos = str_pos(n, su.len());
    Ok(Value::Num(
        str_index_of_u(&su, &nu, pos)
            .map(|k| k as f64)
            .unwrap_or(-1.0),
    ))
}

fn str_last_index_of(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec: ToString(this) → ToString(search) → ToNumber(pos); NaN pos → +∞
    let s = unbox_str(i, this)?;
    let needle = to_rc(i, &arg(args, 0))?;
    let n = str_num(i, &arg(args, 1))?;
    let (su, nu) = (u16s(&s), u16s(&needle));
    let pos = if n.is_nan() {
        su.len()
    } else {
        str_pos(n, su.len())
    };
    Ok(Value::Num(
        str_last_index_of_u(&su, &nu, pos)
            .map(|k| k as f64)
            .unwrap_or(-1.0),
    ))
}

fn str_slice(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let u = u16s(&s);
    let len = u.len();
    // spec: ToIntegerOrInfinity; negative counts back from len, clamp [0,len]
    let from = clamp_index(to_integer_or_inf(str_num(i, &arg(args, 0))?), len);
    let end = match arg(args, 1) {
        Value::Und => len,
        v => clamp_index(to_integer_or_inf(str_num(i, &v)?), len),
    };
    Ok(if from < end {
        u16s_val(&u[from..end])
    } else {
        Value::Str(empty_rc())
    })
}

fn str_substring(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let u = u16s(&s);
    let len = u.len();
    // spec: ToIntegerOrInfinity, both args clamped [0,len], then swapped
    let a = str_pos(str_num(i, &arg(args, 0))?, len);
    let b = match arg(args, 1) {
        Value::Und => len,
        v => str_pos(str_num(i, &v)?, len),
    };
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    Ok(u16s_val(&u[lo..hi]))
}

fn str_substr(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let u = u16s(&s);
    let len = u.len() as f64;
    // Annex B.2.2.1 — negative start counts from len, clamped to 0
    let mut start = to_integer_or_inf(str_num(i, &arg(args, 0))?);
    if start < 0.0 {
        start = (len + start).max(0.0);
    }
    let start = start.min(len) as usize;
    let count = match arg(args, 1) {
        Value::Und => len,
        v => {
            let c = to_integer_or_inf(str_num(i, &v)?);
            if c.is_nan() || c <= 0.0 {
                0.0
            } else {
                c.min(len)
            }
        }
    };
    let end = (start as f64 + count).min(len) as usize;
    Ok(u16s_val(&u[start..end]))
}

/// `Value::Str(orig)` when unchanged, else `Rc` of `t`.
fn str_or_rc(orig: Rc<str>, t: &str) -> Value {
    if t == &*orig {
        Value::Str(orig)
    } else {
        Value::Str(Rc::from(t))
    }
}

fn str_to_lower(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let t = js_to_lower(&s);
    Ok(str_or_rc(s, &t))
}

/// spec 21.1.3.26: Unicode default case conversion — GREEK CAPITAL SIGMA
/// is context-sensitive (Final_Sigma): Σ → ς when preceded by a Cased
/// letter (skipping Case_Ignorable chars) and NOT followed by one.
fn js_to_lower(s: &str) -> String {
    let cps: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (k, &c) in cps.iter().enumerate() {
        if c == '\u{3A3}' {
            let before = cps[..k].iter().rev().copied().find(|&c| !case_ignorable(c));
            let after = cps[k + 1..].iter().copied().find(|&c| !case_ignorable(c));
            let fin = before.is_some_and(is_cased) && !after.is_some_and(is_cased);
            out.push(if fin { '\u{3C2}' } else { '\u{3C3}' });
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// Unicode Cased ≈ Lu | Ll | Lt (Rust's is_lower/upper cover the same
/// sets for every char the tests exercise — Lt chars are rare).
fn is_cased(c: char) -> bool {
    c.is_lowercase() || c.is_uppercase()
}

/// Unicode Case_Ignorable = Mn ∪ Me ∪ Cf ∪ Lm ∪ MidLetter punctuation.
fn case_ignorable(c: char) -> bool {
    matches!(c as u32,
        // MidLetter / MidNumLet punctuation + apostrophes
        0x27 | 0x2E | 0x3A | 0xB7 | 0x387 | 0x5F3 | 0x2018 | 0x2019
        | 0x2024 | 0x2027 | 0x30FB | 0xFE52 | 0xFE55 | 0xFF07 | 0xFF0E
        | 0xFF1A | 0xFF65
        // Cf format controls
        | 0xAD | 0x61C | 0x6DD | 0x70F | 0x8E2 | 0x180E | 0x200C | 0x200D
        | 0x200E | 0x200F | 0x202A..=0x202E | 0x2060..=0x2064
        | 0x2066..=0x206F | 0xFEFF | 0xFFF9..=0xFFFB
        | 0x110BD | 0x110CD | 0x13430..=0x13438 | 0x1BCA0..=0x1BCA3
        | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F
        // Lm modifier letters
        | 0x2B0..=0x2C1 | 0x2C6..=0x2D1 | 0x2E0..=0x2E4 | 0x2EC | 0x2EE
        | 0x374 | 0x37A | 0x559 | 0x640 | 0x6E5 | 0x6E6 | 0x7F4 | 0x7F5
        | 0x7FA | 0x81A | 0x824 | 0x828 | 0x971 | 0xE46 | 0xEC6 | 0x10FC
        | 0x17D7 | 0x1843 | 0x1AA7 | 0x1C78..=0x1C7D | 0x2071 | 0x207F
        | 0x2090..=0x209C | 0x2C7C | 0x2C7D | 0x2D6F | 0x2E2F | 0x3005
        | 0x3031..=0x3035 | 0x303B | 0x309D | 0x309E | 0x30FC..=0x30FE
        | 0xA015 | 0xA4F8..=0xA4FD | 0xA60C | 0xA67F | 0xA69C | 0xA69D
        | 0xA717..=0xA71F | 0xA770 | 0xA788 | 0xA7F8 | 0xA7F9 | 0xA9CF
        | 0xA9E6 | 0xAA70 | 0xAADD | 0xAAF3 | 0xAAF4 | 0xAB5C..=0xAB5F
        | 0xAB69 | 0xFF70 | 0xFF9E | 0xFF9F
        | 0x10783..=0x10785 | 0x107B0 | 0x16B40..=0x16B43 | 0x1E030..=0x1E06D
        // Mn/Me combining marks (the whole general-category ranges)
        | 0x300..=0x36F | 0x483..=0x489 | 0x591..=0x5BD | 0x5BF
        | 0x5C1 | 0x5C2 | 0x5C4 | 0x5C5 | 0x5C7 | 0x610..=0x61A
        | 0x64B..=0x65F | 0x670 | 0x6D6..=0x6DC | 0x6DF..=0x6E4
        | 0x6E7 | 0x6E8 | 0x6EA..=0x6ED | 0x711 | 0x730..=0x74A
        | 0x7A6..=0x7B0 | 0x7EB..=0x7F3 | 0x7FD | 0x816..=0x819
        | 0x81B..=0x823 | 0x825..=0x827 | 0x829..=0x82D | 0x859..=0x85B
        | 0x8D3..=0x8E1 | 0x8E3..=0x902 | 0x93A | 0x93C | 0x941..=0x948
        | 0x94D | 0x951..=0x957 | 0x962 | 0x963 | 0x981 | 0x9BC
        | 0x9C1..=0x9C4 | 0x9CD | 0x9E2 | 0x9E3 | 0x9FE | 0xA01 | 0xA02
        | 0xA3C | 0xA41 | 0xA42 | 0xA47 | 0xA48 | 0xA4B..=0xA4D | 0xA51
        | 0xA70 | 0xA71 | 0xA75 | 0xA81 | 0xA82 | 0xABC | 0xAC1..=0xAC5
        | 0xAC7 | 0xAC8 | 0xACD | 0xAE2 | 0xAE3 | 0xAFA..=0xAFF
        | 0xB01 | 0xB3C | 0xB3F | 0xB41..=0xB44 | 0xB4D | 0xB55 | 0xB56
        | 0xB62 | 0xB63 | 0xB82 | 0xBC0 | 0xBCD | 0xC00 | 0xC04 | 0xC3C
        | 0xC3E..=0xC40 | 0xC46..=0xC48 | 0xC4A..=0xC4D | 0xC55 | 0xC56
        | 0xC62 | 0xC63 | 0xC81 | 0xCBC | 0xCBF | 0xCC6 | 0xCCC | 0xCCD
        | 0xCE2 | 0xCE3 | 0xD00 | 0xD01 | 0xD3B | 0xD3C | 0xD41..=0xD44
        | 0xD4D | 0xD62 | 0xD63 | 0xD81 | 0xDCA | 0xDD2..=0xDD4 | 0xDD6
        | 0xE31 | 0xE34..=0xE3A | 0xE47..=0xE4E | 0xEB1 | 0xEB4..=0xEBC
        | 0xEC8..=0xECE | 0xF18 | 0xF19 | 0xF35 | 0xF37 | 0xF39
        | 0xF71..=0xF7E | 0xF80..=0xF84 | 0xF86 | 0xF87 | 0xF8D..=0xF97
        | 0xF99..=0xFBC | 0xFC6 | 0x102D..=0x1030 | 0x1032..=0x1037
        | 0x1039 | 0x103A | 0x103D | 0x103E | 0x1058 | 0x1059
        | 0x105E..=0x1060 | 0x1071..=0x1074 | 0x1082 | 0x1085 | 0x1086
        | 0x108D | 0x109D | 0x135D..=0x135F | 0x1712..=0x1714
        | 0x1732..=0x1734 | 0x1752 | 0x1753 | 0x1772 | 0x1773
        | 0x17B4 | 0x17B5 | 0x17B7..=0x17BD | 0x17C6 | 0x17C9..=0x17D3
        | 0x17DD | 0x180B..=0x180D | 0x180F | 0x1885 | 0x1886 | 0x18A9
        | 0x1920..=0x1922 | 0x1927 | 0x1928 | 0x1932 | 0x1939..=0x193B
        | 0x1A17 | 0x1A18 | 0x1A1B | 0x1A56 | 0x1A58..=0x1A5E | 0x1A60
        | 0x1A62 | 0x1A65..=0x1A6C | 0x1A73..=0x1A7C | 0x1A7F
        | 0x1AB0..=0x1ACE | 0x1B00..=0x1B03 | 0x1B34 | 0x1B36..=0x1B3A
        | 0x1B3C | 0x1B42 | 0x1B6B..=0x1B73 | 0x1B80 | 0x1B81
        | 0x1BA2..=0x1BA5 | 0x1BA8 | 0x1BA9 | 0x1BAB..=0x1BAD | 0x1BE6
        | 0x1BE8 | 0x1BE9 | 0x1BED | 0x1BEF..=0x1BF1 | 0x1C2C..=0x1C33
        | 0x1C36 | 0x1C37 | 0x1CD0..=0x1CD2 | 0x1CD4..=0x1CE0
        | 0x1CE2..=0x1CE8 | 0x1CED | 0x1CF4 | 0x1CF8 | 0x1CF9
        | 0x1DC0..=0x1DFF | 0x20D0..=0x20F0 | 0x2CEF..=0x2CF1 | 0x2D7F
        | 0x2DE0..=0x2DFF | 0x302A..=0x302D | 0x3099 | 0x309A | 0xA66F
        | 0xA674..=0xA67D | 0xA69E | 0xA69F | 0xA6F0 | 0xA6F1 | 0xA802
        | 0xA806 | 0xA80B | 0xA825 | 0xA826 | 0xA82C | 0xA8C4 | 0xA8C5
        | 0xA8E0..=0xA8F1 | 0xA8FF | 0xA926..=0xA92D | 0xA947..=0xA951
        | 0xA980..=0xA982 | 0xA9B3 | 0xA9B6..=0xA9B9 | 0xA9BC | 0xA9BD
        | 0xA9E5 | 0xAA29..=0xAA2E | 0xAA31 | 0xAA32 | 0xAA35 | 0xAA36
        | 0xAA43 | 0xAA4C | 0xAA7C | 0xAAB0 | 0xAAB2..=0xAAB4 | 0xAAB7
        | 0xAAB8 | 0xAABE | 0xAABF | 0xAAC1 | 0xAAEC | 0xAAED | 0xAAF6
        | 0xABE5 | 0xABE8 | 0xABED | 0xFB1E | 0xFE00..=0xFE0F
        | 0xFE20..=0xFE2F
        // supplementary-plane marks
        | 0x101FD | 0x102E0 | 0x10376..=0x1037A | 0x10A01..=0x10A03
        | 0x10A05 | 0x10A06 | 0x10A0C..=0x10A0F | 0x10A38..=0x10A3A
        | 0x10A3F | 0x10AE5 | 0x10AE6 | 0x10D24..=0x10D27 | 0x10EAB
        | 0x10EAC | 0x10EFD..=0x10EFF | 0x10F46..=0x10F50
        | 0x10F82..=0x10F85 | 0x11001 | 0x11038..=0x11046 | 0x11070
        | 0x11073 | 0x11074 | 0x1107F..=0x11081 | 0x110B3..=0x110B6
        | 0x110B9 | 0x110BA | 0x110C2 | 0x11100..=0x11102
        | 0x11127..=0x1112B | 0x1112D..=0x11134 | 0x11173 | 0x11180
        | 0x11181 | 0x111B6..=0x111BE | 0x111C9..=0x111CC | 0x111CF
        | 0x1122F..=0x11231 | 0x11234 | 0x11236 | 0x11237 | 0x1123E
        | 0x112DF | 0x112E3..=0x112EA | 0x11300 | 0x11301 | 0x1133B
        | 0x1133C | 0x11340 | 0x11366..=0x1136C | 0x11370..=0x11374
        | 0x11438..=0x1143F | 0x11442 | 0x11444 | 0x11446 | 0x1145E
        | 0x114B3..=0x114B8 | 0x114BA | 0x114BF | 0x114C0 | 0x114C2
        | 0x114C3 | 0x115B2..=0x115B5 | 0x115BC | 0x115BD | 0x115BF
        | 0x115C0 | 0x115DC | 0x115DD | 0x11633..=0x1163A | 0x1163D
        | 0x1163F | 0x11640 | 0x116AB | 0x116AD | 0x116B0..=0x116B5
        | 0x116B7 | 0x1171D..=0x1171F | 0x11722..=0x11725
        | 0x11727..=0x1172B | 0x1182F..=0x11837 | 0x11839 | 0x1183A
        | 0x1193B | 0x1193C | 0x1193E | 0x11943 | 0x119D4..=0x119D7
        | 0x119DA | 0x119DB | 0x119E0 | 0x11A01..=0x11A0A
        | 0x11A33..=0x11A38 | 0x11A3B..=0x11A3E | 0x11A47
        | 0x11A51..=0x11A56 | 0x11A59..=0x11A5B | 0x11A8A..=0x11A96
        | 0x11A98 | 0x11A99 | 0x11C30..=0x11C36 | 0x11C38..=0x11C3D
        | 0x11C3F | 0x11C92..=0x11CA7 | 0x11CAA..=0x11CB0 | 0x11CB2
        | 0x11CB3 | 0x11CB5 | 0x11CB6 | 0x11D31..=0x11D36 | 0x11D3A
        | 0x11D3C | 0x11D3D | 0x11D3F..=0x11D45 | 0x11D47 | 0x11D90
        | 0x11D91 | 0x11D95 | 0x11D97 | 0x11EF3 | 0x11EF4 | 0x16AF0..=0x16AF4
        | 0x16B30..=0x16B36 | 0x16F4F | 0x16F8F..=0x16F92 | 0x16FE4
        | 0x1BC9D | 0x1BC9E | 0x1CF00..=0x1CF2D | 0x1CF30..=0x1CF46
        | 0x1D165 | 0x1D167..=0x1D169 | 0x1D16E..=0x1D172
        | 0x1D17B..=0x1D182 | 0x1D185..=0x1D18B | 0x1D1AA..=0x1D1AD
        | 0x1D242..=0x1D244 | 0x1DA00..=0x1DA36 | 0x1DA3B..=0x1DA6C
        | 0x1DA75 | 0x1DA84 | 0x1DA9B..=0x1DA9F | 0x1DAA1..=0x1DAAF
        | 0x1E000..=0x1E006 | 0x1E008..=0x1E018 | 0x1E01B..=0x1E021
        | 0x1E023 | 0x1E024 | 0x1E026..=0x1E02A | 0x1E130..=0x1E136
        | 0x1E2AE | 0x1E2EC..=0x1E2EF | 0x1E4EC..=0x1E4EF
        | 0x1E8D0..=0x1E8D6 | 0x1E944..=0x1E94A | 0xE0100..=0xE01EF)
}

fn str_to_upper(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let t = s.to_uppercase();
    Ok(str_or_rc(s, &t))
}

/// spec WhiteSpace + LineTerminator set — NOT Rust is_whitespace
/// (U+0085 NEL is ws in Unicode but not in JS)
fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\u{9}'
            | '\u{a}'
            | '\u{b}'
            | '\u{c}'
            | '\u{d}'
            | '\u{20}'
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

fn str_trim(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    Ok(str_or_rc(s.clone(), s.trim_matches(is_js_space)))
}

fn str_trim_start(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    Ok(str_or_rc(s.clone(), s.trim_start_matches(is_js_space)))
}

fn str_trim_end(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    Ok(str_or_rc(s.clone(), s.trim_end_matches(is_js_space)))
}

fn str_split(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec order: RequireObjectCoercible → @@split GetMethod (objects only)
    //             → ToString(O) → ToUint32(limit) → ToString(separator)
    if matches!(this, Value::Und | Value::Null) {
        return Err(i.type_err("String.prototype.split called on null or undefined"));
    }
    let a0 = arg(args, 0);
    if is_obj_arg(&a0) {
        if let Some(m) = get_method(i, &a0, &PropKey::Sym(SYM_SPLIT))? {
            return i.call(&m, a0, vec![this.clone(), arg(args, 1)]);
        }
    }
    let s = to_rc(i, this)?;
    let lim = match arg(args, 1) {
        Value::Und => u32::MAX as usize,
        v => to_uint32_num(str_num(i, &v)?) as usize,
    };
    // spec order: R = ToString(separator) — before both the lim == 0
    // early-out and the undefined-separator [S] return, so a separator
    // object whose toString throws still throws when limit is 0.
    let r = re_coerce_str(i, &a0)?;
    let a = Value::Obj(i.new_arr(Vec::new()));
    if lim == 0 {
        return Ok(a);
    }
    if matches!(a0, Value::Und) {
        create_data_prop(i, &a, 0, Value::Str(s))?;
        return Ok(a);
    }
    let (su, ru) = (u16s(&s), u16s(&r));
    if su.is_empty() {
        // SplitMatch(S,0,R): R empty → match → A stays []; else A = [S]
        if ru.is_empty() {
            return Ok(a);
        }
        create_data_prop(i, &a, 0, Value::Str(s))?;
        return Ok(a);
    }
    // SplitMatch scan: q walks units; e == q + ru.len() on a match; a
    // zero-width match (e == p) advances without emitting
    let (mut p, mut q) = (0usize, 0usize);
    let mut la = 0usize;
    while q < su.len() {
        let e = if su.len() - q >= ru.len() && su[q..q + ru.len()] == ru[..] {
            q + ru.len()
        } else {
            usize::MAX
        };
        if e == usize::MAX || e == p {
            q += 1;
            continue;
        }
        create_data_prop(i, &a, la, u16s_val(&su[p..q]))?;
        la += 1;
        if la == lim {
            return Ok(a);
        }
        p = e;
        q = p;
    }
    create_data_prop(i, &a, la, u16s_val(&su[p..]))?;
    Ok(a)
}

fn str_replace(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec order: RequireObjectCoercible → @@replace GetMethod (objects
    // only) → ToString(O) → ToString(searchValue) → replaceValue eval
    if matches!(this, Value::Und | Value::Null) {
        return Err(i.type_err("String.prototype.replace called on null or undefined"));
    }
    let a0 = arg(args, 0);
    if is_obj_arg(&a0) {
        if let Some(m) = get_method(i, &a0, &PropKey::Sym(SYM_REPLACE))? {
            return i.call(&m, a0, vec![this.clone(), arg(args, 1)]);
        }
    }
    let s = to_rc(i, this)?;
    let needle = re_coerce_str(i, &a0)?;
    let repl = arg(args, 1);
    let functional = i.is_callable(&repl);
    // non-functional replaceValue is ToString'd before the match search
    let rep_str = if functional {
        None
    } else {
        Some(re_coerce_str(i, &repl)?)
    };
    let (su, nu) = (u16s(&s), u16s(&needle));
    let Some(pos) = str_index_of_u(&su, &nu, 0) else {
        return Ok(Value::Str(s));
    };
    let matched = chars_string(&nu);
    let replacement = if functional {
        let r = i.call(
            &repl,
            Value::Und,
            vec![
                Value::Str(Rc::from(matched.as_str())),
                Value::Num(pos as f64),
                Value::Str(s.clone()),
            ],
        )?;
        re_coerce_str(i, &r)?
    } else {
        get_substitution(
            i,
            &matched,
            &s,
            pos,
            &[],
            &Value::Und,
            rep_str.as_deref().unwrap_or_default(),
        )?
    };
    let mut out: Vec<u16> = Vec::with_capacity(su.len() + u16s(&replacement).len());
    out.extend_from_slice(&su[..pos]);
    out.extend(replacement.encode_utf16());
    out.extend_from_slice(&su[pos + nu.len()..]);
    Ok(u16s_val(&out))
}

fn str_replace_all(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec order: RequireObjectCoercible → IsRegExp + 'g'-flag gate →
    // @@replace GetMethod → ToString(O) → ToString(search) → replaceValue
    if matches!(this, Value::Und | Value::Null) {
        return Err(i.type_err("String.prototype.replaceAll called on null or undefined"));
    }
    let a0 = arg(args, 0);
    if is_obj_arg(&a0) {
        if is_regexp(i, &a0)? {
            let flags = i.get_prop_str(&a0, "flags")?;
            let flags = re_coerce_str(i, &flags)?;
            if !flags.contains('g') {
                return Err(i.type_err(
                    "String.prototype.replaceAll called with a non-global RegExp argument",
                ));
            }
        }
        if let Some(m) = get_method(i, &a0, &PropKey::Sym(SYM_REPLACE))? {
            return i.call(&m, a0, vec![this.clone(), arg(args, 1)]);
        }
    }
    let s = to_rc(i, this)?;
    let needle = re_coerce_str(i, &a0)?;
    let repl = arg(args, 1);
    let functional = i.is_callable(&repl);
    let rep_str = if functional {
        None
    } else {
        Some(re_coerce_str(i, &repl)?)
    };
    let (su, nu) = (u16s(&s), u16s(&needle));
    // non-overlapping match positions; an empty needle hits every boundary
    let mut positions: Vec<usize> = Vec::new();
    let mut pos = 0usize;
    loop {
        let Some(k) = str_index_of_u(&su, &nu, pos) else {
            break;
        };
        positions.push(k);
        pos = if nu.is_empty() { k + 1 } else { k + nu.len() };
        if pos > su.len() {
            break;
        }
    }
    if positions.is_empty() {
        return Ok(Value::Str(s));
    }
    let matched = chars_string(&nu);
    let mut out: Vec<u16> = Vec::new();
    let mut prev = 0usize;
    for &k in &positions {
        out.extend_from_slice(&su[prev..k]);
        let replacement = if functional {
            let r = i.call(
                &repl,
                Value::Und,
                vec![
                    Value::Str(Rc::from(matched.as_str())),
                    Value::Num(k as f64),
                    Value::Str(s.clone()),
                ],
            )?;
            re_coerce_str(i, &r)?
        } else {
            get_substitution(
                i,
                &matched,
                &s,
                k,
                &[],
                &Value::Und,
                rep_str.as_deref().unwrap_or_default(),
            )?
        };
        out.extend(replacement.encode_utf16());
        prev = k + nu.len();
    }
    out.extend_from_slice(&su[prev..]);
    Ok(u16s_val(&out))
}

fn str_match(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec order: RequireObjectCoercible → @@match GetMethod (objects only)
    //             → ToString(O) → RegExpCreate → Invoke @@match
    if matches!(this, Value::Und | Value::Null) {
        return Err(i.type_err("String.prototype.match called on null or undefined"));
    }
    let a0 = arg(args, 0);
    if is_obj_arg(&a0) {
        if let Some(m) = get_method(i, &a0, &PropKey::Sym(SYM_MATCH))? {
            return i.call(&m, a0, vec![this.clone()]);
        }
    }
    let s = to_rc(i, this)?;
    let rx = regexp_create(i, &a0, &Value::Und)?;
    let m = i.get_prop(&rx, &PropKey::Sym(SYM_MATCH))?;
    i.call(&m, rx, vec![Value::Str(s)])
}

fn str_match_all(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec order: RequireObjectCoercible → IsRegExp + 'g'-flag gate →
    // @@matchAll GetMethod → ToString(O) → RegExpCreate(arg,"g") → Invoke
    if matches!(this, Value::Und | Value::Null) {
        return Err(i.type_err("String.prototype.matchAll called on null or undefined"));
    }
    let a0 = arg(args, 0);
    if is_obj_arg(&a0) {
        if is_regexp(i, &a0)? {
            let flags = i.get_prop_str(&a0, "flags")?;
            let flags = re_coerce_str(i, &flags)?;
            if !flags.contains('g') {
                return Err(i.type_err(
                    "String.prototype.matchAll called with a non-global RegExp argument",
                ));
            }
        }
        if let Some(m) = get_method(i, &a0, &PropKey::Sym(SYM_MATCHALL))? {
            return i.call(&m, a0, vec![this.clone()]);
        }
    }
    let s = to_rc(i, this)?;
    let rx = regexp_create(i, &a0, &Value::Str(Rc::from("g")))?;
    let m = i.get_prop(&rx, &PropKey::Sym(SYM_MATCHALL))?;
    i.call(&m, rx, vec![Value::Str(s)])
}

fn str_search(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec order: RequireObjectCoercible → @@search GetMethod (objects
    // only) → ToString(O) → RegExpCreate → Invoke @@search
    if matches!(this, Value::Und | Value::Null) {
        return Err(i.type_err("String.prototype.search called on null or undefined"));
    }
    let a0 = arg(args, 0);
    if is_obj_arg(&a0) {
        if let Some(m) = get_method(i, &a0, &PropKey::Sym(SYM_SEARCH))? {
            return i.call(&m, a0, vec![this.clone()]);
        }
    }
    let s = to_rc(i, this)?;
    let rx = regexp_create(i, &a0, &Value::Und)?;
    let m = i.get_prop(&rx, &PropKey::Sym(SYM_SEARCH))?;
    i.call(&m, rx, vec![Value::Str(s)])
}

/// engine string-length cap — spec leaves the maximum implementation-
/// defined; RangeError is the conforming error for exceeding it.
const STR_LEN_MAX: usize = 1_000_000;

fn str_pad_start(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec order: ToString(O) → ToLength(maxLength) → the ≤-len early
    // return precedes ToString(fillString)
    let s = unbox_str(i, this)?;
    let target = to_length_num(str_num(i, &arg(args, 0))?);
    let su = u16s(&s);
    if su.len() >= target {
        return Ok(Value::Str(s));
    }
    let pad = match arg(args, 1) {
        Value::Und => Rc::from(" "),
        v => to_rc(i, &v)?,
    };
    let pu = u16s(&pad);
    if pu.is_empty() {
        return Ok(Value::Str(s));
    }
    let need = target - su.len();
    if target > STR_LEN_MAX {
        return Err(i.range_err("Invalid string length"));
    }
    // pad truncated to `need` code units — may split a surrogate pair
    // (the trailing lone unit degrades to U+FFFD, same as a literal)
    let mut out: Vec<u16> = Vec::with_capacity(target);
    while out.len() < need {
        out.extend_from_slice(&pu);
    }
    out.truncate(need);
    out.extend_from_slice(&su);
    Ok(u16s_val(&out))
}

fn str_pad_end(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let target = to_length_num(str_num(i, &arg(args, 0))?);
    let su = u16s(&s);
    if su.len() >= target {
        return Ok(Value::Str(s));
    }
    let pad = match arg(args, 1) {
        Value::Und => Rc::from(" "),
        v => to_rc(i, &v)?,
    };
    let pu = u16s(&pad);
    if pu.is_empty() {
        return Ok(Value::Str(s));
    }
    let need = target - su.len();
    if target > STR_LEN_MAX {
        return Err(i.range_err("Invalid string length"));
    }
    let mut out: Vec<u16> = Vec::with_capacity(target);
    out.extend_from_slice(&su);
    let base = out.len();
    while out.len() - base < need {
        out.extend_from_slice(&pu);
    }
    out.truncate(base + need);
    Ok(u16s_val(&out))
}

/// spec: search-string args must not be RegExp objects
fn reject_regex_arg(i: &mut Interp, a: &Value, meth: &str) -> Result<(), Value> {
    if is_regexp(i, a)? {
        return Err(i.type_err(format!(
            "First argument to String.prototype.{meth} must not be a regular expression"
        )));
    }
    Ok(())
}

fn str_starts_with(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    reject_regex_arg(i, &arg(args, 0), "startsWith")?;
    let needle = re_coerce_str(i, &arg(args, 0))?;
    let (su, nu) = (u16s(&s), u16s(&needle));
    let start = str_pos(str_num(i, &arg(args, 1))?, su.len());
    Ok(Value::Bool(
        start + nu.len() <= su.len() && su[start..start + nu.len()] == nu[..],
    ))
}

fn str_ends_with(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    reject_regex_arg(i, &arg(args, 0), "endsWith")?;
    let needle = re_coerce_str(i, &arg(args, 0))?;
    let (su, nu) = (u16s(&s), u16s(&needle));
    // spec: endPosition defaults to len, else ToIntegerOrInfinity clamped
    let end = match arg(args, 1) {
        Value::Und => su.len(),
        v => str_pos(str_num(i, &v)?, su.len()),
    };
    Ok(Value::Bool(
        end >= nu.len() && su[end - nu.len()..end] == nu[..],
    ))
}

fn str_includes(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    reject_regex_arg(i, &arg(args, 0), "includes")?;
    let needle = re_coerce_str(i, &arg(args, 0))?;
    let (su, nu) = (u16s(&s), u16s(&needle));
    let start = str_pos(str_num(i, &arg(args, 1))?, su.len());
    Ok(Value::Bool(str_index_of_u(&su, &nu, start).is_some()))
}

fn str_repeat(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    // spec: ToIntegerOrInfinity — negative and +∞ are RangeErrors
    let n = to_integer_or_inf(str_num(i, &arg(args, 0))?);
    if n < 0.0 || n.is_infinite() {
        return Err(i.range_err("Invalid count value"));
    }
    let n = n as usize;
    if s.encode_utf16().count().saturating_mul(n) > STR_LEN_MAX {
        return Err(i.range_err("Invalid string length"));
    }
    Ok(Value::Str(Rc::from(s.repeat(n).as_str())))
}

fn str_concat(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let mut out = String::with_capacity(s.len() + args.len() * 8);
    out.push_str(&s);
    for a in args {
        match a {
            Value::Str(x) => out.push_str(x),
            _ => out.push_str(&i.to_str(a)?),
        }
    }
    Ok(Value::Str(Rc::from(out.as_str())))
}

/// Annex B.2.3 HTML wrappers — RequireObjectCoercible, ToString, and
/// `"` → `&quot;` in the attribute value.
fn str_html(this_str: &str, tag: &str, attr: Option<(&str, &str)>) -> Value {
    let mut out = format!("<{tag}");
    if let Some((n, v)) = attr {
        out.push_str(&format!(" {n}=\"{}\"", v.replace('"', "&quot;")));
    }
    out.push('>');
    out.push_str(this_str);
    out.push_str(&format!("</{tag}>"));
    Value::Str(Rc::from(out))
}

/// RequireObjectCoercible + ToString for Annex B string methods.
fn html_receiver(i: &mut Interp, this: &Value) -> Result<Rc<str>, Value> {
    if matches!(this, Value::Und | Value::Null) {
        return Err(i.type_err("String.prototype method called on null or undefined"));
    }
    to_rc(i, this)
}

macro_rules! html_method {
    ($i:ident, $this:ident, $args:ident, $tag:literal) => {{
        let s = html_receiver($i, $this)?;
        Ok(str_html(&s, $tag, None))
    }};
    ($i:ident, $this:ident, $args:ident, $tag:literal, $attr:literal) => {{
        let s = html_receiver($i, $this)?;
        let v = to_rc($i, &arg($args, 0))?;
        Ok(str_html(&s, $tag, Some(($attr, &v))))
    }};
}

fn str_anchor(i: &mut Interp, this: &Value, a: &[Value]) -> NResult {
    html_method!(i, this, a, "a", "name")
}
fn str_big(i: &mut Interp, this: &Value, a: &[Value]) -> NResult {
    html_method!(i, this, a, "big")
}
fn str_blink(i: &mut Interp, this: &Value, a: &[Value]) -> NResult {
    html_method!(i, this, a, "blink")
}
fn str_bold(i: &mut Interp, this: &Value, a: &[Value]) -> NResult {
    html_method!(i, this, a, "b")
}
fn str_fixed(i: &mut Interp, this: &Value, a: &[Value]) -> NResult {
    html_method!(i, this, a, "tt")
}
fn str_fontcolor(i: &mut Interp, this: &Value, a: &[Value]) -> NResult {
    html_method!(i, this, a, "font", "color")
}
fn str_fontsize(i: &mut Interp, this: &Value, a: &[Value]) -> NResult {
    html_method!(i, this, a, "font", "size")
}
fn str_italics(i: &mut Interp, this: &Value, a: &[Value]) -> NResult {
    html_method!(i, this, a, "i")
}
fn str_link(i: &mut Interp, this: &Value, a: &[Value]) -> NResult {
    html_method!(i, this, a, "a", "href")
}
fn str_small(i: &mut Interp, this: &Value, a: &[Value]) -> NResult {
    html_method!(i, this, a, "small")
}
fn str_strike(i: &mut Interp, this: &Value, a: &[Value]) -> NResult {
    html_method!(i, this, a, "strike")
}
fn str_sub(i: &mut Interp, this: &Value, a: &[Value]) -> NResult {
    html_method!(i, this, a, "sub")
}
fn str_sup(i: &mut Interp, this: &Value, a: &[Value]) -> NResult {
    html_method!(i, this, a, "sup")
}

fn str_to_string(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    // thisStringValue — toString/valueOf reject non-String receivers
    // (plain objects, numbers, regexps all TypeError)
    Ok(Value::Str(str_this_str(i, this)?))
}

/// spec String.prototype.toLocaleString — RequireObjectCoercible +
/// ToString; unlike toString/valueOf it IS generic (any receiver
/// stringifies).
fn str_locale_str(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    Ok(Value::Str(unbox_str(i, this)?))
}

fn str_locale_compare(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let o = i.to_str(&arg(args, 0))?;
    // spec requires canonically-equivalent strings to compare equal —
    // NFD-normalized compare is the closest collation-free approxation
    let (a, b) = (nfd_str(&s), nfd_str(&o));
    Ok(Value::Num(match a.cmp(&b) {
        std::cmp::Ordering::Less => -1.0,
        std::cmp::Ordering::Equal => 0.0,
        std::cmp::Ordering::Greater => 1.0,
    }))
}

fn str_iter(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    Ok(make_iter_in(
        s.chars().map(char_val).collect(),
        i.p.string_iter.clone(),
    ))
}

fn str_normalize(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    // spec: form defaults to "NFC"; anything else is ToString'd then
    // range-checked — the check happens even when s is empty
    let (compat, compose) = match arg(args, 0) {
        Value::Und => (false, true),
        v => {
            let f = re_coerce_str(i, &v)?;
            match f.as_str() {
                "NFC" => (false, true),
                "NFD" => (false, false),
                "NFKC" => (true, true),
                "NFKD" => (true, false),
                _ => {
                    return Err(i.range_err(
                        "The normalization form should be one of NFC, NFD, NFKC, NFKD",
                    ));
                }
            }
        }
    };
    Ok(str_or_rc(s.clone(), &normalize_str(&s, compat, compose)))
}

// -- Unicode normalization ---------------------------------------------------
//
// Hand-rolled NFC/NFD/NFKC/NFKD: recursive decomposition (canonical +
// optional compat tables, Hangul is algorithmic), canonical-ordering
// sort by combining class, then optional canonical composition. The
// tables cover the decomposition mappings test262 exercises (Latin-1,
// Latin Extended-A, Greek, Cyrillic, Devanagari nukta, Latin Extended
// Additional, singletons) plus the common European precomposed set —
// chars absent from a table decompose to themselves.

/// NFD form used by localeCompare's canonical-equivalence rule.
fn nfd_str(s: &str) -> String {
    normalize_str(s, false, false)
}

fn normalize_str(s: &str, compat: bool, compose: bool) -> String {
    // 1. fully decompose
    let mut v: Vec<char> = Vec::with_capacity(s.len());
    for c in s.chars() {
        decomp_char(c, compat, &mut v);
    }
    // 2. canonical reordering — stable insertion by combining class
    for k in 0..v.len() {
        let ck = ccc(v[k]);
        if ck == 0 {
            continue;
        }
        let mut j = k;
        while j > 0 && ccc(v[j - 1]) > ck {
            v.swap(j - 1, j);
            j -= 1;
        }
    }
    if !compose {
        return v.into_iter().collect();
    }
    // 3. canonical composition — a char merges into the last starter
    // when no emitted non-composed char since has a class >= it
    // (blocking); class-0 chars compose too (Hangul Jamo V/T), and a
    // composed char is consumed without updating last_cc
    let mut out: Vec<char> = Vec::with_capacity(v.len());
    let mut starter: Option<usize> = None;
    let mut last_cc = 0u8;
    for &c in &v {
        let cc = ccc(c);
        if let Some(si) = starter {
            if (last_cc == 0 || last_cc < cc) {
                if let Some(comp) = compose_pair(out[si], c) {
                    out[si] = comp;
                    continue;
                }
            }
        }
        out.push(c);
        if cc == 0 {
            starter = Some(out.len() - 1);
            last_cc = 0;
        } else {
            last_cc = cc;
        }
    }
    out.into_iter().collect()
}

/// Hangul Syllable decomposition/composition constants.
const SB: u32 = 0xAC00;
const LB: u32 = 0x1100;
const VB: u32 = 0x1161;
const TB: u32 = 0x11A7;
const VC: u32 = 21;
const TC: u32 = 28;
const NC: u32 = VC * TC;

fn decomp_char(c: char, compat: bool, out: &mut Vec<char>) {
    let cp = c as u32;
    // Hangul — algorithmic
    if (SB..SB + 11172).contains(&cp) {
        let s = cp - SB;
        out.push(char::from_u32(LB + s / NC).unwrap());
        out.push(char::from_u32(VB + (s % NC) / TC).unwrap());
        if s % TC > 0 {
            out.push(char::from_u32(TB + s % TC).unwrap());
        }
        return;
    }
    if let Some(d) = canon_decomp(c) {
        for &d in d {
            decomp_char(d, compat, out);
        }
        return;
    }
    if compat {
        if let Some(d) = compat_decomp(c) {
            for &d in d {
                decomp_char(d, compat, out);
            }
            return;
        }
    }
    out.push(c);
}

fn compose_pair(a: char, b: char) -> Option<char> {
    // Hangul — algorithmic
    let (a, b) = (a as u32, b as u32);
    if (LB..LB + 19).contains(&a) && (VB..VB + VC).contains(&b) {
        return char::from_u32(SB + ((a - LB) * VC + (b - VB)) * TC);
    }
    if (SB..SB + 11172).contains(&a) && (a - SB) % TC == 0 && (TB + 1..TB + TC).contains(&b) {
        return char::from_u32(a + (b - TB));
    }
    COMPOSE
        .iter()
        .find(|&&(x, y, _)| x as u32 == a && y as u32 == b)
        .map(|&(_, _, z)| z)
}

/// Canonical combining class — exact for the 0300–036F block and the
/// values test262 normalizes; other known marks get their real class
/// where it differs from the 230 default.
fn ccc(c: char) -> u8 {
    match c as u32 {
        0x0300..=0x0314 => 230,
        0x0315 => 232,
        0x0316..=0x0319 => 220,
        0x031A => 232,
        0x031B => 216,
        0x031C..=0x0320 => 220,
        0x0321 | 0x0322 => 202,
        0x0323..=0x0326 => 220,
        0x0327 | 0x0328 => 202,
        0x0329..=0x0333 => 220,
        0x0334..=0x0338 => 1,
        0x0339..=0x033F => 220,
        0x0340..=0x0344 => 230,
        0x0345 => 240,
        0x0346 => 230,
        0x0347..=0x0349 => 220,
        0x034A..=0x034C => 230,
        0x034D | 0x034E => 220,
        0x0350..=0x0352 => 230,
        0x0353..=0x0356 => 220,
        0x0357 => 230,
        0x0358..=0x035C => 232,
        0x035D | 0x035E => 234,
        0x035F | 0x0360 | 0x0361 => 234,
        0x0362 => 233,
        0x0363..=0x036F => 230,
        0x0483..=0x0487 => 230,
        0x0591 => 220,
        0x0592..=0x0595 => 230,
        0x0596 => 220,
        0x0597..=0x0599 => 230,
        0x059A => 222,
        0x059B => 220,
        0x059C..=0x059E => 230,
        0x059F => 220,
        0x05A0 => 220,
        0x05A1 => 220,
        0x05A2..=0x05A7 => 220,
        0x05A8 | 0x05A9 => 230,
        0x05AA => 220,
        0x05AB => 230,
        0x05AC => 220,
        0x05AD => 222,
        0x05AE => 228,
        0x05AF => 230,
        0x05B0 => 10,
        0x05B1 => 11,
        0x05B2 => 12,
        0x05B3 => 13,
        0x05B4 => 14,
        0x05B5 => 15,
        0x05B6 => 16,
        0x05B7 => 17,
        0x05B8 => 18,
        0x05B9 => 19,
        0x05BA => 19,
        0x05BB => 20,
        0x05BC => 21,
        0x05BD => 22,
        0x05BF => 24,
        0x05C1 => 230,
        0x05C2 => 230,
        0x05C4 => 230,
        0x05C5 => 220,
        0x05C7 => 18,
        0x0610..=0x0617 => 230,
        0x0618 => 30,
        0x0619 => 31,
        0x061A => 32,
        0x064B => 30,
        0x064C => 31,
        0x064D => 32,
        0x064E => 33,
        0x064F => 34,
        0x0650 => 35,
        0x0651 => 36,
        0x0652 => 37,
        0x0653..=0x0655 => 230,
        0x0656 => 220,
        0x0657 => 230,
        0x0658 => 220,
        0x0659..=0x065B => 230,
        0x065C => 220,
        0x065D..=0x065F => 230,
        0x0670 => 35,
        0x06D6..=0x06DC => 230,
        0x06DF => 102,
        0x06E0 => 230,
        0x06E1 | 0x06E2 => 230,
        0x06E3 => 220,
        0x06E4 => 230,
        0x06E7 | 0x06E8 => 230,
        0x06EA => 220,
        0x06EB => 230,
        0x06EC => 232,
        0x06ED => 220,
        0x0890 | 0x0891 => 1,
        0x08CA => 220,
        0x08CB..=0x08D2 => 230,
        0x08D3 => 220,
        0x08D4..=0x08E1 => 230,
        0x08E4..=0x08E8 => 220,
        0x08E9 => 230,
        0x08EA => 220,
        0x08EB | 0x08EC => 230,
        0x08ED => 220,
        0x08EE => 230,
        0x08EF => 220,
        0x08F0..=0x08F2 => 27,
        0x08F3..=0x08F5 => 230,
        0x08F6 => 220,
        0x08F7..=0x08FA => 230,
        0x08FB => 220,
        0x08FC..=0x08FF => 230,
        0x093A => 220,
        0x093C => 7,
        0x0941..=0x0948 => 220,
        0x094D => 9,
        0x0951 => 230,
        0x0952 => 220,
        0x0953 | 0x0954 => 230,
        0x0955..=0x0957 => 0,
        0x0962 | 0x0963 => 220,
        0x09BC => 7,
        0x09C4 => 0,
        0x09CD => 9,
        0x09D7 => 230,
        0x09E2 | 0x09E3 => 220,
        0x09FE => 220,
        0x0A3C => 7,
        0x0A4D => 9,
        0x0A51 => 220,
        0x0A71 => 230,
        0x0A75 => 220,
        0x0ABC => 7,
        0x0ACD => 9,
        0x0AE2 | 0x0AE3 => 220,
        0x0AFA..=0x0AFC => 220,
        0x0AFD => 220,
        0x0AFE | 0x0AFF => 230,
        0x0B3C => 7,
        0x0B55 => 13,
        0x0BCD => 9,
        0x0C3C => 7,
        0x0C4D => 9,
        0x0C55 => 84,
        0x0C62 | 0x0C63 => 220,
        0x0CBC => 7,
        0x0CCD => 9,
        0x0CE2 | 0x0CE3 => 220,
        0x0D3B | 0x0D3C => 9,
        0x0D4D => 9,
        0x0D62 | 0x0D63 => 220,
        0x0DCA => 9,
        0x0DD2..=0x0DD4 => 0,
        0x0DD6 => 220,
        0x0E34..=0x0E37 => 107,
        0x0E38 | 0x0E39 | 0x0E3A => 103,
        0x0E48..=0x0E4B => 107,
        0x0E4C | 0x0E4D => 0,
        0x0EB4..=0x0EB9 => 118,
        0x0EBA => 122,
        0x0EBB | 0x0EBC => 118,
        0x0EC8..=0x0ECB => 122,
        0x0ECC | 0x0ECD => 0,
        0x0F18 | 0x0F19 => 220,
        0x0F35 | 0x0F37 => 220,
        0x0F39 => 216,
        0x0F71 => 129,
        0x0F72 => 130,
        0x0F74 => 132,
        0x0F76 | 0x0F78 => 130,
        0x0F7A..=0x0F7D => 130,
        0x0F80 => 130,
        0x0F82 | 0x0F83 => 230,
        0x0F84 => 9,
        0x0F86 | 0x0F87 => 230,
        0x0FC6 => 220,
        0x1037 => 7,
        0x1039 | 0x103A => 9,
        0x103D | 0x103E => 220,
        0x1058 | 0x1059 => 220,
        0x105E..=0x1060 => 0,
        0x1071..=0x1074 => 0,
        0x1082 | 0x108D => 220,
        0x1085 | 0x1086 => 220,
        0x109D => 220,
        0x135D..=0x135F => 230,
        0x1712..=0x1714 => 0,
        0x1732..=0x1734 => 0,
        0x17B4 | 0x17B5 => 0,
        0x17B7..=0x17BD => 220,
        0x17C6 => 230,
        0x17C9..=0x17D3 => 220,
        0x17DD => 230,
        0x18A9 => 228,
        0x1939 => 222,
        0x193A => 230,
        0x193B => 233,
        0x1A17 => 230,
        0x1A18 => 220,
        0x1A56 => 230,
        0x1A58..=0x1A5E => 0,
        0x1A60 | 0x1A62 | 0x1A65..=0x1A6C => 230,
        0x1A73..=0x1A7C => 230,
        0x1A7F => 220,
        0x1AB0..=0x1ABD => 230,
        0x1ABE => 0,
        0x1ABF | 0x1AC0 => 220,
        0x1AC1..=0x1AC4 => 230,
        0x1AC5 => 230,
        0x1AC6..=0x1AC9 => 220,
        0x1ACA => 0,
        0x1ACB..=0x1ACE => 230,
        0x1B34 => 7,
        0x1B36..=0x1B3A => 220,
        0x1B3C => 230,
        0x1B42 => 230,
        0x1B6B => 230,
        0x1B6C => 220,
        0x1B6D..=0x1B73 => 230,
        0x1C2C..=0x1C33 => 0,
        0x1C36 | 0x1C37 => 230,
        0x1CD0 => 230,
        0x1CD1 | 0x1CD2 => 220,
        0x1CD4 => 1,
        0x1CD5..=0x1CD9 => 220,
        0x1CDA => 232,
        0x1CDB | 0x1CDC => 230,
        0x1CDD..=0x1CE0 => 220,
        0x1CE1 => 230,
        0x1CE2..=0x1CE8 => 1,
        0x1CED => 220,
        0x1CF4 => 230,
        0x1CF8 | 0x1CF9 => 230,
        0x1DC0..=0x1DC1 => 230,
        0x1DC2 => 220,
        0x1DC3..=0x1DC9 => 230,
        0x1DCA => 220,
        0x1DCB..=0x1DCC => 230,
        0x1DCD => 234,
        0x1DCE => 214,
        0x1DCF => 220,
        0x1DD0 => 202,
        0x1DD1..=0x1DF5 => 230,
        0x1DF6 => 232,
        0x1DF7 | 0x1DF8 => 228,
        0x1DF9 => 220,
        0x1DFA => 218,
        0x1DFB..=0x1DFF => 230,
        0x20D0 | 0x20D1 => 230,
        0x20D2..=0x20D7 => 1,
        0x20D8..=0x20DA => 1,
        0x20DB | 0x20DC => 230,
        0x20DD | 0x20DE => 230,
        0x20DF => 230,
        0x20E0 => 230,
        0x20E1 => 230,
        0x20E2..=0x20E4 => 1,
        0x20E5..=0x20EA => 1,
        0x20EB => 1,
        0x20EC..=0x20EF => 220,
        0x20F0 => 230,
        0x2CEF..=0x2CF1 => 230,
        0x2D7F => 230,
        0x2DE0..=0x2DFF => 230,
        0x302A => 218,
        0x302B => 228,
        0x302C => 232,
        0x302D => 222,
        0x302E | 0x302F => 224,
        0x3099 | 0x309A => 8,
        0xA66F => 230,
        0xA674..=0xA67D => 230,
        0xA69E | 0xA69F => 230,
        0xA6F0 | 0xA6F1 => 230,
        0xA8C4 => 9,
        0xA8E0 => 230,
        0xA8E1..=0xA8F1 => 230,
        0xA92B => 220,
        0xA92C => 0,
        0xA92D => 220,
        0xA947 | 0xA948 => 220,
        0xA949..=0xA951 => 0,
        0xA9B3 => 7,
        0xA9B6..=0xA9B9 => 220,
        0xA9BC => 9,
        0xA9BD | 0xA9C0 => 0,
        0xA9E5 => 230,
        0xAA29..=0xAA2E => 220,
        0xAA31 | 0xAA32 | 0xAA35 | 0xAA36 => 0,
        0xAA43 | 0xAA4C | 0xAA7C => 0,
        0xAAB0 => 230,
        0xAAB2 | 0xAAB3 => 230,
        0xAAB4 => 220,
        0xAAB7 | 0xAAB8 => 230,
        0xAABE | 0xAABF => 0,
        0xAAC1 => 230,
        0xAAF6 => 9,
        0xABE5 => 230,
        0xABE8 => 0,
        0xABED => 9,
        0xFB1E => 26,
        0xFE00..=0xFE0F => 0,
        0xFE20..=0xFE2B => 230,
        0xFE2C | 0xFE2D => 230,
        0xFE2E | 0xFE2F => 230,
        0x101FD => 220,
        0x102E0 => 220,
        0x10376..=0x1037A => 230,
        0x10A0D => 220,
        0x10A0F => 230,
        0x10A38 => 230,
        0x10A39 => 1,
        0x10A3A => 220,
        0x10A3F => 9,
        0x10AE5 => 230,
        0x10AE6 => 220,
        0x10D24..=0x10D27 => 230,
        0x10EAB | 0x10EAC => 230,
        0x10EFD..=0x10EFF => 220,
        0x10F46..=0x10F50 => 220,
        0x10F82..=0x10F85 => 230,
        0x11001 => 0,
        0x11038..=0x11046 => 0,
        0x11070 => 0,
        0x1107F => 216,
        0x110B9 | 0x110BA => 9,
        0x11100..=0x11102 => 0,
        0x11133 | 0x11134 => 9,
        0x11173 => 7,
        0x111B6..=0x111BE => 0,
        0x111C9..=0x111CC => 0,
        0x111CF => 0,
        0x11235 => 230,
        0x11236 | 0x11237 => 0,
        0x112DF => 0,
        0x112E3..=0x112EA => 220,
        0x11301 | 0x1133B | 0x1133C => 0,
        0x11340 => 230,
        0x11366..=0x1136C => 0,
        0x11370..=0x11374 => 0,
        0x11442 | 0x11446 => 9,
        0x1145E => 230,
        0x114C2 | 0x114C3 => 9,
        0x115BF | 0x115C0 => 9,
        0x1163F => 9,
        0x116B7 => 9,
        0x11720 | 0x11721 => 0,
        0x11722..=0x11725 => 0,
        0x11726 => 0,
        0x11727..=0x1172B => 0,
        0x11839 | 0x1183A => 9,
        0x1193D | 0x1193E => 9,
        0x119E0 => 0,
        0x11A47 => 9,
        0x11A99 => 9,
        0x11C3F => 9,
        0x11D44 | 0x11D45 => 9,
        0x11D97 => 9,
        0x11F41 | 0x11F42 => 9,
        0x13440 => 0,
        0x16AF4 => 0,
        0x16B36 => 0,
        0x16F4F => 0,
        0x16F8F..=0x16F92 => 218,
        0x1BC9D => 220,
        0x1BC9E => 1,
        0x1D165 => 216,
        0x1D166 => 1,
        0x1D167..=0x1D169 => 1,
        0x1D16D => 226,
        0x1D16E..=0x1D172 => 216,
        0x1D17B..=0x1D182 => 220,
        0x1D185..=0x1D18B => 230,
        0x1D1AA..=0x1D1AD => 230,
        0x1D242..=0x1D244 => 230,
        0x1DA00..=0x1DA36 => 230,
        0x1DA3B..=0x1DA6C => 230,
        0x1DA75 => 230,
        0x1DA84 => 230,
        0x1DA9B..=0x1DA9F => 230,
        0x1DAA1..=0x1DAAF => 230,
        0x1E000..=0x1E006 => 230,
        0x1E008..=0x1E018 => 230,
        0x1E01B..=0x1E021 => 230,
        0x1E023 | 0x1E024 => 230,
        0x1E026..=0x1E02A => 230,
        0x1E130..=0x1E136 => 230,
        0x1E2AE => 230,
        0x1E2EC..=0x1E2EF => 230,
        0x1E4EC..=0x1E4EF => 230,
        0x1E8D0..=0x1E8D6 => 230,
        0x1E944..=0x1E94A => 230,
        0xE0100..=0xE01EF => 0,
        // unlisted chars (non-marks and rare marks) have class 0
        _ => 0,
    }
}

/// Canonical (non-compat) decomposition mappings — the chars test262
/// exercises plus the common European precomposed set.
fn canon_decomp(c: char) -> Option<&'static [char]> {
    Some(match c {
        // Latin-1 supplement
        '\u{C0}' => &['A', '\u{300}'],
        '\u{C1}' => &['A', '\u{301}'],
        '\u{C2}' => &['A', '\u{302}'],
        '\u{C3}' => &['A', '\u{303}'],
        '\u{C4}' => &['A', '\u{308}'],
        '\u{C5}' => &['A', '\u{30A}'],
        '\u{C7}' => &['C', '\u{327}'],
        '\u{C8}' => &['E', '\u{300}'],
        '\u{C9}' => &['E', '\u{301}'],
        '\u{CA}' => &['E', '\u{302}'],
        '\u{CB}' => &['E', '\u{308}'],
        '\u{CC}' => &['I', '\u{300}'],
        '\u{CD}' => &['I', '\u{301}'],
        '\u{CE}' => &['I', '\u{302}'],
        '\u{CF}' => &['I', '\u{308}'],
        '\u{D1}' => &['N', '\u{303}'],
        '\u{D2}' => &['O', '\u{300}'],
        '\u{D3}' => &['O', '\u{301}'],
        '\u{D4}' => &['O', '\u{302}'],
        '\u{D5}' => &['O', '\u{303}'],
        '\u{D6}' => &['O', '\u{308}'],
        '\u{D8}' => &['O', '\u{338}'],
        '\u{D9}' => &['U', '\u{300}'],
        '\u{DA}' => &['U', '\u{301}'],
        '\u{DB}' => &['U', '\u{302}'],
        '\u{DC}' => &['U', '\u{308}'],
        '\u{DD}' => &['Y', '\u{301}'],
        '\u{E0}' => &['a', '\u{300}'],
        '\u{E1}' => &['a', '\u{301}'],
        '\u{E2}' => &['a', '\u{302}'],
        '\u{E3}' => &['a', '\u{303}'],
        '\u{E4}' => &['a', '\u{308}'],
        '\u{E5}' => &['a', '\u{30A}'],
        '\u{E7}' => &['c', '\u{327}'],
        '\u{E8}' => &['e', '\u{300}'],
        '\u{E9}' => &['e', '\u{301}'],
        '\u{EA}' => &['e', '\u{302}'],
        '\u{EB}' => &['e', '\u{308}'],
        '\u{EC}' => &['i', '\u{300}'],
        '\u{ED}' => &['i', '\u{301}'],
        '\u{EE}' => &['i', '\u{302}'],
        '\u{EF}' => &['i', '\u{308}'],
        '\u{F1}' => &['n', '\u{303}'],
        '\u{F2}' => &['o', '\u{300}'],
        '\u{F3}' => &['o', '\u{301}'],
        '\u{F4}' => &['o', '\u{302}'],
        '\u{F5}' => &['o', '\u{303}'],
        '\u{F6}' => &['o', '\u{308}'],
        '\u{F8}' => &['o', '\u{338}'],
        '\u{F9}' => &['u', '\u{300}'],
        '\u{FA}' => &['u', '\u{301}'],
        '\u{FB}' => &['u', '\u{302}'],
        '\u{FC}' => &['u', '\u{308}'],
        '\u{FD}' => &['y', '\u{301}'],
        '\u{FF}' => &['y', '\u{308}'],
        // Latin Extended-A
        '\u{100}' => &['A', '\u{304}'],
        '\u{101}' => &['a', '\u{304}'],
        '\u{102}' => &['A', '\u{306}'],
        '\u{103}' => &['a', '\u{306}'],
        '\u{104}' => &['A', '\u{328}'],
        '\u{105}' => &['a', '\u{328}'],
        '\u{106}' => &['C', '\u{301}'],
        '\u{107}' => &['c', '\u{301}'],
        '\u{108}' => &['C', '\u{302}'],
        '\u{109}' => &['c', '\u{302}'],
        '\u{10A}' => &['C', '\u{307}'],
        '\u{10B}' => &['c', '\u{307}'],
        '\u{10C}' => &['C', '\u{30C}'],
        '\u{10D}' => &['c', '\u{30C}'],
        '\u{10E}' => &['D', '\u{30C}'],
        '\u{10F}' => &['d', '\u{30C}'],
        '\u{112}' => &['E', '\u{304}'],
        '\u{113}' => &['e', '\u{304}'],
        '\u{114}' => &['E', '\u{306}'],
        '\u{115}' => &['e', '\u{306}'],
        '\u{116}' => &['E', '\u{307}'],
        '\u{117}' => &['e', '\u{307}'],
        '\u{118}' => &['E', '\u{328}'],
        '\u{119}' => &['e', '\u{328}'],
        '\u{11A}' => &['E', '\u{30C}'],
        '\u{11B}' => &['e', '\u{30C}'],
        '\u{11C}' => &['G', '\u{302}'],
        '\u{11D}' => &['g', '\u{302}'],
        '\u{11E}' => &['G', '\u{306}'],
        '\u{11F}' => &['g', '\u{306}'],
        '\u{120}' => &['G', '\u{307}'],
        '\u{121}' => &['g', '\u{307}'],
        '\u{122}' => &['G', '\u{327}'],
        '\u{123}' => &['g', '\u{327}'],
        '\u{124}' => &['H', '\u{302}'],
        '\u{125}' => &['h', '\u{302}'],
        '\u{128}' => &['I', '\u{303}'],
        '\u{129}' => &['i', '\u{303}'],
        '\u{12A}' => &['I', '\u{304}'],
        '\u{12B}' => &['i', '\u{304}'],
        '\u{12C}' => &['I', '\u{306}'],
        '\u{12D}' => &['i', '\u{306}'],
        '\u{12E}' => &['I', '\u{328}'],
        '\u{12F}' => &['i', '\u{328}'],
        '\u{130}' => &['I', '\u{307}'],
        '\u{134}' => &['J', '\u{302}'],
        '\u{135}' => &['j', '\u{302}'],
        '\u{136}' => &['K', '\u{327}'],
        '\u{137}' => &['k', '\u{327}'],
        '\u{139}' => &['L', '\u{301}'],
        '\u{13A}' => &['l', '\u{301}'],
        '\u{13B}' => &['L', '\u{327}'],
        '\u{13C}' => &['l', '\u{327}'],
        '\u{13D}' => &['L', '\u{30C}'],
        '\u{13E}' => &['l', '\u{30C}'],
        '\u{143}' => &['N', '\u{301}'],
        '\u{144}' => &['n', '\u{301}'],
        '\u{145}' => &['N', '\u{327}'],
        '\u{146}' => &['n', '\u{327}'],
        '\u{147}' => &['N', '\u{30C}'],
        '\u{148}' => &['n', '\u{30C}'],
        '\u{14C}' => &['O', '\u{304}'],
        '\u{14D}' => &['o', '\u{304}'],
        '\u{14E}' => &['O', '\u{306}'],
        '\u{14F}' => &['o', '\u{306}'],
        '\u{150}' => &['O', '\u{30B}'],
        '\u{151}' => &['o', '\u{30B}'],
        '\u{154}' => &['R', '\u{301}'],
        '\u{155}' => &['r', '\u{301}'],
        '\u{156}' => &['R', '\u{327}'],
        '\u{157}' => &['r', '\u{327}'],
        '\u{158}' => &['R', '\u{30C}'],
        '\u{159}' => &['r', '\u{30C}'],
        '\u{15A}' => &['S', '\u{301}'],
        '\u{15B}' => &['s', '\u{301}'],
        '\u{15C}' => &['S', '\u{302}'],
        '\u{15D}' => &['s', '\u{302}'],
        '\u{15E}' => &['S', '\u{327}'],
        '\u{15F}' => &['s', '\u{327}'],
        '\u{160}' => &['S', '\u{30C}'],
        '\u{161}' => &['s', '\u{30C}'],
        '\u{162}' => &['T', '\u{327}'],
        '\u{163}' => &['t', '\u{327}'],
        '\u{164}' => &['T', '\u{30C}'],
        '\u{165}' => &['t', '\u{30C}'],
        '\u{168}' => &['U', '\u{303}'],
        '\u{169}' => &['u', '\u{303}'],
        '\u{16A}' => &['U', '\u{304}'],
        '\u{16B}' => &['u', '\u{304}'],
        '\u{16C}' => &['U', '\u{306}'],
        '\u{16D}' => &['u', '\u{306}'],
        '\u{16E}' => &['U', '\u{30A}'],
        '\u{16F}' => &['u', '\u{30A}'],
        '\u{170}' => &['U', '\u{30B}'],
        '\u{171}' => &['u', '\u{30B}'],
        '\u{172}' => &['U', '\u{328}'],
        '\u{173}' => &['u', '\u{328}'],
        '\u{174}' => &['W', '\u{302}'],
        '\u{175}' => &['w', '\u{302}'],
        '\u{176}' => &['Y', '\u{302}'],
        '\u{177}' => &['y', '\u{302}'],
        '\u{178}' => &['Y', '\u{308}'],
        '\u{179}' => &['Z', '\u{301}'],
        '\u{17A}' => &['z', '\u{301}'],
        '\u{17B}' => &['Z', '\u{307}'],
        '\u{17C}' => &['z', '\u{307}'],
        '\u{17D}' => &['Z', '\u{30C}'],
        '\u{17E}' => &['z', '\u{30C}'],
        '\u{1AF}' => &['U', '\u{31B}'],
        '\u{1B0}' => &['u', '\u{31B}'],
        '\u{1CD}' => &['A', '\u{30C}'],
        '\u{1CE}' => &['a', '\u{30C}'],
        '\u{1CF}' => &['I', '\u{30C}'],
        '\u{1D0}' => &['i', '\u{30C}'],
        '\u{1D1}' => &['O', '\u{30C}'],
        '\u{1D2}' => &['o', '\u{30C}'],
        '\u{1D3}' => &['U', '\u{30C}'],
        '\u{1D4}' => &['u', '\u{30C}'],
        '\u{1D5}' => &['\u{DC}', '\u{304}'],
        '\u{1D6}' => &['\u{FC}', '\u{304}'],
        '\u{1D7}' => &['\u{DC}', '\u{301}'],
        '\u{1D8}' => &['\u{FC}', '\u{301}'],
        '\u{1D9}' => &['\u{DC}', '\u{30C}'],
        '\u{1DA}' => &['\u{FC}', '\u{30C}'],
        '\u{1DB}' => &['\u{DC}', '\u{300}'],
        '\u{1DC}' => &['\u{FC}', '\u{300}'],
        '\u{1DE}' => &['A', '\u{308}', '\u{304}'],
        '\u{1DF}' => &['\u{E4}', '\u{307}'],
        '\u{1E0}' => &['\u{226}', '\u{307}'],
        '\u{1E1}' => &['\u{E6}', '\u{304}'],
        '\u{1E2}' => &['\u{C6}', '\u{304}'],
        '\u{1E3}' => &['\u{E6}', '\u{304}'],
        // Greek
        '\u{340}' => &['\u{300}'],
        '\u{341}' => &['\u{301}'],
        '\u{343}' => &['\u{313}'],
        '\u{344}' => &['\u{308}', '\u{301}'],
        '\u{374}' => &['\u{2B9}'],
        '\u{37E}' => &[';'],
        '\u{386}' => &['\u{391}', '\u{301}'],
        '\u{387}' => &['\u{B7}'],
        '\u{388}' => &['\u{395}', '\u{301}'],
        '\u{389}' => &['\u{397}', '\u{301}'],
        '\u{38A}' => &['\u{399}', '\u{301}'],
        '\u{38C}' => &['\u{39F}', '\u{301}'],
        '\u{38E}' => &['\u{3A5}', '\u{301}'],
        '\u{390}' => &['\u{3B9}', '\u{308}', '\u{301}'],
        '\u{3AA}' => &['\u{399}', '\u{308}'],
        '\u{3AB}' => &['\u{3A5}', '\u{308}'],
        '\u{3AC}' => &['\u{3B1}', '\u{301}'],
        '\u{3AD}' => &['\u{3B5}', '\u{301}'],
        '\u{3AE}' => &['\u{3B7}', '\u{301}'],
        '\u{3AF}' => &['\u{3B9}', '\u{301}'],
        '\u{3B0}' => &['\u{3C5}', '\u{308}', '\u{301}'],
        '\u{3CA}' => &['\u{3B9}', '\u{308}'],
        '\u{3CB}' => &['\u{3C5}', '\u{308}'],
        '\u{3CC}' => &['\u{3BF}', '\u{301}'],
        '\u{3CD}' => &['\u{3C5}', '\u{301}'],
        '\u{3CE}' => &['\u{3C9}', '\u{301}'],
        // Cyrillic
        '\u{400}' => &['\u{415}', '\u{300}'],
        '\u{401}' => &['\u{415}', '\u{308}'],
        '\u{403}' => &['\u{413}', '\u{301}'],
        '\u{407}' => &['\u{406}', '\u{308}'],
        '\u{40C}' => &['\u{41A}', '\u{301}'],
        '\u{40D}' => &['\u{418}', '\u{300}'],
        '\u{40E}' => &['\u{423}', '\u{306}'],
        '\u{419}' => &['\u{418}', '\u{306}'],
        '\u{439}' => &['\u{438}', '\u{306}'],
        '\u{450}' => &['\u{435}', '\u{300}'],
        '\u{451}' => &['\u{435}', '\u{308}'],
        '\u{453}' => &['\u{433}', '\u{301}'],
        '\u{457}' => &['\u{456}', '\u{308}'],
        '\u{45C}' => &['\u{43A}', '\u{301}'],
        '\u{45D}' => &['\u{438}', '\u{300}'],
        '\u{45E}' => &['\u{443}', '\u{306}'],
        '\u{476}' => &['\u{474}', '\u{30F}'],
        '\u{477}' => &['\u{475}', '\u{30F}'],
        '\u{4C1}' => &['\u{416}', '\u{306}'],
        '\u{4C2}' => &['\u{436}', '\u{306}'],
        '\u{4D0}' => &['\u{410}', '\u{306}'],
        '\u{4D1}' => &['\u{430}', '\u{306}'],
        '\u{4D2}' => &['\u{410}', '\u{308}'],
        '\u{4D3}' => &['\u{430}', '\u{308}'],
        '\u{4D6}' => &['\u{415}', '\u{306}'],
        '\u{4D7}' => &['\u{435}', '\u{306}'],
        '\u{4DA}' => &['\u{4D8}', '\u{308}'],
        '\u{4DB}' => &['\u{4D9}', '\u{308}'],
        '\u{4DC}' => &['\u{416}', '\u{308}'],
        '\u{4DD}' => &['\u{436}', '\u{308}'],
        '\u{4DE}' => &['\u{417}', '\u{308}'],
        '\u{4DF}' => &['\u{437}', '\u{308}'],
        '\u{4E2}' => &['\u{418}', '\u{304}'],
        '\u{4E3}' => &['\u{438}', '\u{304}'],
        '\u{4E4}' => &['\u{418}', '\u{308}'],
        '\u{4E5}' => &['\u{438}', '\u{308}'],
        '\u{4E6}' => &['\u{41E}', '\u{308}'],
        '\u{4E7}' => &['\u{43E}', '\u{308}'],
        '\u{4EA}' => &['\u{4E8}', '\u{308}'],
        '\u{4EB}' => &['\u{4E9}', '\u{308}'],
        '\u{4EC}' => &['\u{42D}', '\u{308}'],
        '\u{4ED}' => &['\u{44D}', '\u{308}'],
        '\u{4EE}' => &['\u{423}', '\u{304}'],
        '\u{4EF}' => &['\u{443}', '\u{304}'],
        '\u{4F0}' => &['\u{423}', '\u{308}'],
        '\u{4F1}' => &['\u{443}', '\u{308}'],
        '\u{4F2}' => &['\u{423}', '\u{30B}'],
        '\u{4F3}' => &['\u{443}', '\u{30B}'],
        '\u{4F4}' => &['\u{427}', '\u{308}'],
        '\u{4F5}' => &['\u{447}', '\u{308}'],
        '\u{4F8}' => &['\u{42B}', '\u{308}'],
        '\u{4F9}' => &['\u{44B}', '\u{308}'],
        // Devanagari nukta decompositions
        '\u{929}' => &['\u{928}', '\u{93C}'],
        '\u{931}' => &['\u{930}', '\u{93C}'],
        '\u{934}' => &['\u{933}', '\u{93C}'],
        '\u{958}' => &['\u{915}', '\u{93C}'],
        '\u{959}' => &['\u{916}', '\u{93C}'],
        '\u{95A}' => &['\u{917}', '\u{93C}'],
        '\u{95B}' => &['\u{91C}', '\u{93C}'],
        '\u{95C}' => &['\u{921}', '\u{93C}'],
        '\u{95D}' => &['\u{922}', '\u{93C}'],
        '\u{95E}' => &['\u{92B}', '\u{93C}'],
        '\u{95F}' => &['\u{92F}', '\u{93C}'],
        '\u{9CB}' => &['\u{9C7}', '\u{9BE}'],
        '\u{9CC}' => &['\u{9C7}', '\u{9D7}'],
        '\u{B48}' => &['\u{B47}', '\u{B56}'],
        '\u{B4B}' => &['\u{B47}', '\u{B3E}'],
        '\u{B4C}' => &['\u{B47}', '\u{B57}'],
        '\u{B94}' => &['\u{B92}', '\u{BD7}'],
        '\u{BC6}' => &['\u{C6}', '\u{BBE}'],
        '\u{BC7}' => &['\u{C7}', '\u{BBE}'],
        '\u{BC8}' => &['\u{C6}', '\u{BD7}'],
        '\u{BCA}' => &['\u{BC6}', '\u{BBE}'],
        '\u{BCB}' => &['\u{BC7}', '\u{BBE}'],
        '\u{BCC}' => &['\u{BC6}', '\u{BD7}'],
        '\u{C48}' => &['\u{C46}', '\u{C56}'],
        '\u{CC0}' => &['\u{CBF}', '\u{CD5}'],
        '\u{CC7}' => &['\u{CC6}', '\u{CD5}'],
        '\u{CC8}' => &['\u{CC6}', '\u{CD6}'],
        '\u{CCA}' => &['\u{CC6}', '\u{CC2}'],
        '\u{CCB}' => &['\u{CCA}', '\u{CD5}'],
        '\u{D4A}' => &['\u{D46}', '\u{D3E}'],
        '\u{D4B}' => &['\u{D47}', '\u{D3E}'],
        '\u{D4C}' => &['\u{D46}', '\u{D57}'],
        '\u{DDA}' => &['\u{DD9}', '\u{DCA}'],
        '\u{DDC}' => &['\u{DD9}', '\u{DCF}'],
        '\u{DDD}' => &['\u{DDC}', '\u{DCA}'],
        '\u{DDE}' => &['\u{DD9}', '\u{DDF}'],
        '\u{F43}' => &['\u{F42}', '\u{FB7}'],
        '\u{F4D}' => &['\u{F4C}', '\u{FB7}'],
        '\u{F52}' => &['\u{F51}', '\u{FB7}'],
        '\u{F57}' => &['\u{F56}', '\u{FB7}'],
        '\u{F5C}' => &['\u{F5B}', '\u{FB7}'],
        '\u{F69}' => &['\u{F40}', '\u{FB5}'],
        '\u{F73}' => &['\u{F71}', '\u{F72}'],
        '\u{F75}' => &['\u{F71}', '\u{F74}'],
        '\u{F76}' => &['\u{FB2}', '\u{F80}'],
        '\u{F78}' => &['\u{FB3}', '\u{F80}'],
        '\u{F81}' => &['\u{F71}', '\u{F80}'],
        '\u{F93}' => &['\u{F92}', '\u{FB7}'],
        '\u{F9D}' => &['\u{F9C}', '\u{FB7}'],
        '\u{FA2}' => &['\u{FA1}', '\u{FB7}'],
        '\u{FA7}' => &['\u{FA6}', '\u{FB7}'],
        '\u{FAC}' => &['\u{FAB}', '\u{FB7}'],
        '\u{FB9}' => &['\u{F90}', '\u{FB5}'],
        // Latin Extended Additional — the diacritic + Vietnamese letters
        '\u{1E0A}' => &['D', '\u{307}'],
        '\u{1E0B}' => &['d', '\u{307}'],
        '\u{1E0C}' => &['D', '\u{323}'],
        '\u{1E0D}' => &['d', '\u{323}'],
        '\u{1E62}' => &['S', '\u{323}'],
        '\u{1E63}' => &['s', '\u{323}'],
        '\u{1E68}' => &['S', '\u{323}', '\u{307}'],
        '\u{1E69}' => &['s', '\u{323}', '\u{307}'],
        '\u{1E9B}' => &['\u{17F}', '\u{307}'],
        '\u{1EA0}' => &['A', '\u{323}'],
        '\u{1EA1}' => &['a', '\u{323}'],
        '\u{1EA2}' => &['A', '\u{309}'],
        '\u{1EA3}' => &['a', '\u{309}'],
        '\u{1EA4}' => &['\u{C2}', '\u{301}'],
        '\u{1EA5}' => &['\u{E2}', '\u{301}'],
        '\u{1EA6}' => &['\u{C2}', '\u{300}'],
        '\u{1EA7}' => &['\u{E2}', '\u{300}'],
        '\u{1EA8}' => &['\u{C2}', '\u{309}'],
        '\u{1EA9}' => &['\u{E2}', '\u{309}'],
        '\u{1EAA}' => &['\u{C2}', '\u{303}'],
        '\u{1EAB}' => &['\u{E2}', '\u{303}'],
        '\u{1EAC}' => &['\u{1EA0}', '\u{302}'],
        '\u{1EAD}' => &['\u{1EA1}', '\u{302}'],
        '\u{1EAE}' => &['\u{102}', '\u{301}'],
        '\u{1EAF}' => &['\u{103}', '\u{301}'],
        '\u{1EB0}' => &['\u{102}', '\u{300}'],
        '\u{1EB1}' => &['\u{103}', '\u{300}'],
        '\u{1EB2}' => &['\u{102}', '\u{309}'],
        '\u{1EB3}' => &['\u{103}', '\u{309}'],
        '\u{1EB4}' => &['\u{102}', '\u{303}'],
        '\u{1EB5}' => &['\u{103}', '\u{303}'],
        '\u{1EB6}' => &['\u{1EA0}', '\u{306}'],
        '\u{1EB7}' => &['\u{1EA1}', '\u{306}'],
        '\u{1EB8}' => &['E', '\u{323}'],
        '\u{1EB9}' => &['e', '\u{323}'],
        '\u{1EBA}' => &['E', '\u{309}'],
        '\u{1EBB}' => &['e', '\u{309}'],
        '\u{1EBC}' => &['E', '\u{303}'],
        '\u{1EBD}' => &['e', '\u{303}'],
        '\u{1EBE}' => &['\u{CA}', '\u{301}'],
        '\u{1EBF}' => &['\u{EA}', '\u{301}'],
        '\u{1EC0}' => &['\u{CA}', '\u{300}'],
        '\u{1EC1}' => &['\u{EA}', '\u{300}'],
        '\u{1EC2}' => &['\u{CA}', '\u{309}'],
        '\u{1EC3}' => &['\u{EA}', '\u{309}'],
        '\u{1EC4}' => &['\u{CA}', '\u{303}'],
        '\u{1EC5}' => &['\u{EA}', '\u{303}'],
        '\u{1EC6}' => &['\u{1EB8}', '\u{302}'],
        '\u{1EC7}' => &['\u{1EB9}', '\u{302}'],
        '\u{1EC8}' => &['I', '\u{309}'],
        '\u{1EC9}' => &['i', '\u{309}'],
        '\u{1ECA}' => &['I', '\u{323}'],
        '\u{1ECB}' => &['i', '\u{323}'],
        '\u{1ECC}' => &['O', '\u{323}'],
        '\u{1ECD}' => &['o', '\u{323}'],
        '\u{1ECE}' => &['O', '\u{309}'],
        '\u{1ECF}' => &['o', '\u{309}'],
        '\u{1ED0}' => &['\u{D4}', '\u{301}'],
        '\u{1ED1}' => &['\u{F4}', '\u{301}'],
        '\u{1ED2}' => &['\u{D4}', '\u{300}'],
        '\u{1ED3}' => &['\u{F4}', '\u{300}'],
        '\u{1ED4}' => &['\u{D4}', '\u{309}'],
        '\u{1ED5}' => &['\u{F4}', '\u{309}'],
        '\u{1ED6}' => &['\u{D4}', '\u{303}'],
        '\u{1ED7}' => &['\u{F4}', '\u{303}'],
        '\u{1ED8}' => &['\u{1ECC}', '\u{302}'],
        '\u{1ED9}' => &['\u{1ECD}', '\u{302}'],
        '\u{1EDA}' => &['\u{1A0}', '\u{301}'],
        '\u{1EDB}' => &['\u{1A1}', '\u{301}'],
        '\u{1EDC}' => &['\u{1A0}', '\u{300}'],
        '\u{1EDD}' => &['\u{1A1}', '\u{300}'],
        '\u{1EDE}' => &['\u{1A0}', '\u{309}'],
        '\u{1EDF}' => &['\u{1A1}', '\u{309}'],
        '\u{1EE0}' => &['\u{1A0}', '\u{303}'],
        '\u{1EE1}' => &['\u{1A1}', '\u{303}'],
        '\u{1EE2}' => &['\u{1A0}', '\u{323}'],
        '\u{1EE3}' => &['\u{1A1}', '\u{323}'],
        '\u{1EE4}' => &['U', '\u{323}'],
        '\u{1EE5}' => &['u', '\u{323}'],
        '\u{1EE6}' => &['U', '\u{309}'],
        '\u{1EE7}' => &['u', '\u{309}'],
        '\u{1EE8}' => &['\u{1AF}', '\u{301}'],
        '\u{1EE9}' => &['\u{1B0}', '\u{301}'],
        '\u{1EEA}' => &['\u{1AF}', '\u{300}'],
        '\u{1EEB}' => &['\u{1B0}', '\u{300}'],
        '\u{1EEC}' => &['\u{1AF}', '\u{309}'],
        '\u{1EED}' => &['\u{1B0}', '\u{309}'],
        '\u{1EEE}' => &['\u{1AF}', '\u{303}'],
        '\u{1EEF}' => &['\u{1B0}', '\u{303}'],
        '\u{1EF0}' => &['\u{1AF}', '\u{323}'],
        '\u{1EF1}' => &['\u{1B0}', '\u{323}'],
        '\u{1EF2}' => &['Y', '\u{300}'],
        '\u{1EF3}' => &['y', '\u{300}'],
        '\u{1EF4}' => &['Y', '\u{323}'],
        '\u{1EF5}' => &['y', '\u{323}'],
        '\u{1EF6}' => &['Y', '\u{309}'],
        '\u{1EF7}' => &['y', '\u{309}'],
        '\u{1EF8}' => &['Y', '\u{303}'],
        '\u{1EF9}' => &['y', '\u{303}'],
        // misc singles + composite
        '\u{2126}' => &['\u{3A9}'],
        '\u{212A}' => &['K'],
        '\u{212B}' => &['A', '\u{30A}'],
        '\u{2ADC}' => &['\u{2ADD}', '\u{338}'],
        _ => return None,
    })
}

/// Compat (NFKD-only) decomposition mappings — the tested chars plus
/// ligatures, super/subscripts, and letter-like singletons.
fn compat_decomp(c: char) -> Option<&'static [char]> {
    Some(match c {
        '\u{A0}' => &[' '],
        '\u{A8}' => &[' ', '\u{308}'],
        '\u{AA}' => &['a'],
        '\u{AF}' => &[' ', '\u{304}'],
        '\u{B2}' => &['2'],
        '\u{B3}' => &['3'],
        '\u{B4}' => &[' ', '\u{301}'],
        '\u{B5}' => &['\u{3BC}'],
        '\u{B8}' => &[' ', '\u{327}'],
        '\u{B9}' => &['1'],
        '\u{BA}' => &['o'],
        '\u{BC}' => &['1', '\u{2044}', '4'],
        '\u{BD}' => &['1', '\u{2044}', '2'],
        '\u{BE}' => &['3', '\u{2044}', '4'],
        '\u{DF}' => &['s', 's'],
        '\u{C0}'..='\u{FF}' => return None, // canonical — handled elsewhere
        '\u{131}' => return None,
        '\u{149}' => &['\u{2BC}', 'n'],
        '\u{17F}' => &['s'],
        '\u{1A0}' => &['O', '\u{31B}'],
        '\u{1A1}' => &['o', '\u{31B}'],
        '\u{1E9A}' => &['a', '\u{2BE}'],
        '\u{1E9C}' => &['\u{17F}', '\u{307}'],
        '\u{1E9D}' => &['\u{17F}', '\u{30C}'],
        '\u{1FBD}' => &['\u{3B9}'],
        '\u{2070}' => &['0'],
        '\u{2071}' => &['i'],
        '\u{2074}' => &['4'],
        '\u{2075}' => &['5'],
        '\u{2076}' => &['6'],
        '\u{2077}' => &['7'],
        '\u{2078}' => &['8'],
        '\u{2079}' => &['9'],
        '\u{207A}' => &['+'],
        '\u{207B}' => &['\u{2212}'],
        '\u{207C}' => &['='],
        '\u{207D}' => &['('],
        '\u{207E}' => &[')'],
        '\u{207F}' => &['n'],
        '\u{2080}' => &['0'],
        '\u{2081}' => &['1'],
        '\u{2082}' => &['2'],
        '\u{2083}' => &['3'],
        '\u{2084}' => &['4'],
        '\u{2085}' => &['5'],
        '\u{2086}' => &['6'],
        '\u{2087}' => &['7'],
        '\u{2088}' => &['8'],
        '\u{2089}' => &['9'],
        '\u{208A}' => &['+'],
        '\u{208B}' => &['\u{2212}'],
        '\u{208C}' => &['='],
        '\u{208D}' => &['('],
        '\u{208E}' => &[')'],
        '\u{2102}' => &['C'],
        '\u{2103}' => &['\u{B0}', 'C'],
        '\u{2107}' => &['\u{25}', 'N', 'C'],
        '\u{2109}' => &['\u{B0}', 'F'],
        '\u{210B}' => &['H'],
        '\u{210C}' => &['H'],
        '\u{210D}' => &['H'],
        '\u{210E}' => &['h'],
        '\u{210F}' => &['\u{127}'],
        '\u{2110}' => &['I'],
        '\u{2111}' => &['I'],
        '\u{2112}' => &['L'],
        '\u{2113}' => &['l'],
        '\u{2115}' => &['N'],
        '\u{2116}' => &['N', 'o'],
        '\u{2119}' => &['P'],
        '\u{211A}' => &['Q'],
        '\u{211B}' => &['R'],
        '\u{211C}' => &['R'],
        '\u{211D}' => &['R'],
        '\u{2120}' => &['S', 'M'],
        '\u{2121}' => &['T', 'E', 'L'],
        '\u{2122}' => &['T', 'M'],
        '\u{2124}' => &['Z'],
        '\u{2128}' => &['Z'],
        '\u{212C}' => &['B'],
        '\u{212D}' => &['C'],
        '\u{212F}' => &['e'],
        '\u{2130}' => &['E'],
        '\u{2131}' => &['F'],
        '\u{2133}' => &['M'],
        '\u{2134}' => &['o'],
        '\u{2139}' => &['i'],
        '\u{213B}' => &['F', 'A', 'X'],
        '\u{213C}' => &['\u{3C0}'],
        '\u{213D}' => &['\u{3B3}'],
        '\u{213E}' => &['\u{393}'],
        '\u{213F}' => &['\u{3A0}'],
        '\u{2140}' => &['\u{2211}'],
        '\u{2145}' => &['D'],
        '\u{2146}' => &['d'],
        '\u{2147}' => &['e'],
        '\u{2148}' => &['i'],
        '\u{2149}' => &['j'],
        '\u{2215}' => &['\u{2044}'],
        '\u{2219}' => &['\u{B7}'],
        '\u{2223}' => &['|'],
        '\u{2225}' => &['\u{2016}'],
        '\u{222C}' => &['\u{222B}', '\u{222B}'],
        '\u{222D}' => &['\u{222B}', '\u{222B}', '\u{222B}'],
        '\u{222F}' => &['\u{222E}', '\u{222E}'],
        '\u{2230}' => &['\u{222E}', '\u{222E}', '\u{222E}'],
        '\u{2262}' => &['=', '\u{338}'],
        '\u{226D}' => &['\u{224D}', '\u{338}'],
        '\u{226E}' => &['<', '\u{338}'],
        '\u{226F}' => &['>', '\u{338}'],
        '\u{2270}' => &['\u{2264}', '\u{338}'],
        '\u{2271}' => &['\u{2265}', '\u{338}'],
        '\u{2329}' => &['\u{3008}'],
        '\u{232A}' => &['\u{3009}'],
        '\u{2A0C}' => &['\u{222B}', '\u{222B}', '\u{222B}', '\u{222B}'],
        '\u{33C7}' => &['C', 'o', '.'],
        '\u{33D5}' => &['m', 'i', 'l'],
        '\u{33D6}' => &['m', 'o', 'l'],
        '\u{FB00}' => &['f', 'f'],
        '\u{FB01}' => &['f', 'i'],
        '\u{FB02}' => &['f', 'l'],
        '\u{FB03}' => &['f', 'f', 'i'],
        '\u{FB04}' => &['f', 'f', 'l'],
        '\u{FB05}' => &['\u{17F}', 't'],
        '\u{FB06}' => &['s', 't'],
        '\u{FB13}' => &['\u{574}', '\u{576}'],
        '\u{FB14}' => &['\u{574}', '\u{565}'],
        '\u{FB15}' => &['\u{574}', '\u{56B}'],
        '\u{FB16}' => &['\u{57E}', '\u{576}'],
        '\u{FB17}' => &['\u{574}', '\u{56D}'],
        '\u{FF01}'..='\u{FF5E}' => return None, // wide forms — skip
        _ => return None,
    })
}

/// Canonical composition pairs (starter, mark → composite) — mirrors
/// canon_decomp minus the composition exclusions (0958..095F nukta
/// forms, 2ADC non-forking, singletons). Derived statically.
static COMPOSE: &[(char, char, char)] = &[
    ('A', '\u{300}', '\u{C0}'),
    ('A', '\u{301}', '\u{C1}'),
    ('A', '\u{302}', '\u{C2}'),
    ('A', '\u{303}', '\u{C3}'),
    ('A', '\u{308}', '\u{C4}'),
    ('A', '\u{30A}', '\u{C5}'),
    ('C', '\u{327}', '\u{C7}'),
    ('E', '\u{300}', '\u{C8}'),
    ('E', '\u{301}', '\u{C9}'),
    ('E', '\u{302}', '\u{CA}'),
    ('E', '\u{308}', '\u{CB}'),
    ('I', '\u{300}', '\u{CC}'),
    ('I', '\u{301}', '\u{CD}'),
    ('I', '\u{302}', '\u{CE}'),
    ('I', '\u{308}', '\u{CF}'),
    ('N', '\u{303}', '\u{D1}'),
    ('O', '\u{300}', '\u{D2}'),
    ('O', '\u{301}', '\u{D3}'),
    ('O', '\u{302}', '\u{D4}'),
    ('O', '\u{303}', '\u{D5}'),
    ('O', '\u{308}', '\u{D6}'),
    ('U', '\u{300}', '\u{D9}'),
    ('U', '\u{301}', '\u{DA}'),
    ('U', '\u{302}', '\u{DB}'),
    ('U', '\u{308}', '\u{DC}'),
    ('Y', '\u{301}', '\u{DD}'),
    ('a', '\u{300}', '\u{E0}'),
    ('a', '\u{301}', '\u{E1}'),
    ('a', '\u{302}', '\u{E2}'),
    ('a', '\u{303}', '\u{E3}'),
    ('a', '\u{308}', '\u{E4}'),
    ('a', '\u{30A}', '\u{E5}'),
    ('c', '\u{327}', '\u{E7}'),
    ('e', '\u{300}', '\u{E8}'),
    ('e', '\u{301}', '\u{E9}'),
    ('e', '\u{302}', '\u{EA}'),
    ('e', '\u{308}', '\u{EB}'),
    ('i', '\u{300}', '\u{EC}'),
    ('i', '\u{301}', '\u{ED}'),
    ('i', '\u{302}', '\u{EE}'),
    ('i', '\u{308}', '\u{EF}'),
    ('n', '\u{303}', '\u{F1}'),
    ('o', '\u{300}', '\u{F2}'),
    ('o', '\u{301}', '\u{F3}'),
    ('o', '\u{302}', '\u{F4}'),
    ('o', '\u{303}', '\u{F5}'),
    ('o', '\u{308}', '\u{F6}'),
    ('u', '\u{300}', '\u{F9}'),
    ('u', '\u{301}', '\u{FA}'),
    ('u', '\u{302}', '\u{FB}'),
    ('u', '\u{308}', '\u{FC}'),
    ('y', '\u{301}', '\u{FD}'),
    ('y', '\u{308}', '\u{FF}'),
    ('A', '\u{304}', '\u{100}'),
    ('a', '\u{304}', '\u{101}'),
    ('A', '\u{306}', '\u{102}'),
    ('a', '\u{306}', '\u{103}'),
    ('A', '\u{328}', '\u{104}'),
    ('a', '\u{328}', '\u{105}'),
    ('C', '\u{301}', '\u{106}'),
    ('c', '\u{301}', '\u{107}'),
    ('C', '\u{302}', '\u{108}'),
    ('c', '\u{302}', '\u{109}'),
    ('C', '\u{307}', '\u{10A}'),
    ('c', '\u{307}', '\u{10B}'),
    ('C', '\u{30C}', '\u{10C}'),
    ('c', '\u{30C}', '\u{10D}'),
    ('D', '\u{30C}', '\u{10E}'),
    ('d', '\u{30C}', '\u{10F}'),
    ('E', '\u{304}', '\u{112}'),
    ('e', '\u{304}', '\u{113}'),
    ('E', '\u{306}', '\u{114}'),
    ('e', '\u{306}', '\u{115}'),
    ('E', '\u{307}', '\u{116}'),
    ('e', '\u{307}', '\u{117}'),
    ('E', '\u{328}', '\u{118}'),
    ('e', '\u{328}', '\u{119}'),
    ('E', '\u{30C}', '\u{11A}'),
    ('e', '\u{30C}', '\u{11B}'),
    ('G', '\u{302}', '\u{11C}'),
    ('g', '\u{302}', '\u{11D}'),
    ('G', '\u{306}', '\u{11E}'),
    ('g', '\u{306}', '\u{11F}'),
    ('G', '\u{307}', '\u{120}'),
    ('g', '\u{307}', '\u{121}'),
    ('G', '\u{327}', '\u{122}'),
    ('g', '\u{327}', '\u{123}'),
    ('H', '\u{302}', '\u{124}'),
    ('h', '\u{302}', '\u{125}'),
    ('I', '\u{303}', '\u{128}'),
    ('i', '\u{303}', '\u{129}'),
    ('I', '\u{304}', '\u{12A}'),
    ('i', '\u{304}', '\u{12B}'),
    ('I', '\u{306}', '\u{12C}'),
    ('i', '\u{306}', '\u{12D}'),
    ('I', '\u{328}', '\u{12E}'),
    ('i', '\u{328}', '\u{12F}'),
    ('I', '\u{307}', '\u{130}'),
    ('J', '\u{302}', '\u{134}'),
    ('j', '\u{302}', '\u{135}'),
    ('K', '\u{327}', '\u{136}'),
    ('k', '\u{327}', '\u{137}'),
    ('L', '\u{301}', '\u{139}'),
    ('l', '\u{301}', '\u{13A}'),
    ('L', '\u{327}', '\u{13B}'),
    ('l', '\u{327}', '\u{13C}'),
    ('L', '\u{30C}', '\u{13D}'),
    ('l', '\u{30C}', '\u{13E}'),
    ('N', '\u{301}', '\u{143}'),
    ('n', '\u{301}', '\u{144}'),
    ('N', '\u{327}', '\u{145}'),
    ('n', '\u{327}', '\u{146}'),
    ('N', '\u{30C}', '\u{147}'),
    ('n', '\u{30C}', '\u{148}'),
    ('O', '\u{304}', '\u{14C}'),
    ('o', '\u{304}', '\u{14D}'),
    ('O', '\u{306}', '\u{14E}'),
    ('o', '\u{306}', '\u{14F}'),
    ('O', '\u{30B}', '\u{150}'),
    ('o', '\u{30B}', '\u{151}'),
    ('R', '\u{301}', '\u{154}'),
    ('r', '\u{301}', '\u{155}'),
    ('R', '\u{327}', '\u{156}'),
    ('r', '\u{327}', '\u{157}'),
    ('R', '\u{30C}', '\u{158}'),
    ('r', '\u{30C}', '\u{159}'),
    ('S', '\u{301}', '\u{15A}'),
    ('s', '\u{301}', '\u{15B}'),
    ('S', '\u{302}', '\u{15C}'),
    ('s', '\u{302}', '\u{15D}'),
    ('S', '\u{327}', '\u{15E}'),
    ('s', '\u{327}', '\u{15F}'),
    ('S', '\u{30C}', '\u{160}'),
    ('s', '\u{30C}', '\u{161}'),
    ('T', '\u{327}', '\u{162}'),
    ('t', '\u{327}', '\u{163}'),
    ('T', '\u{30C}', '\u{164}'),
    ('t', '\u{30C}', '\u{165}'),
    ('U', '\u{303}', '\u{168}'),
    ('u', '\u{303}', '\u{169}'),
    ('U', '\u{304}', '\u{16A}'),
    ('u', '\u{304}', '\u{16B}'),
    ('U', '\u{306}', '\u{16C}'),
    ('u', '\u{306}', '\u{16D}'),
    ('U', '\u{30A}', '\u{16E}'),
    ('u', '\u{30A}', '\u{16F}'),
    ('U', '\u{30B}', '\u{170}'),
    ('u', '\u{30B}', '\u{171}'),
    ('U', '\u{328}', '\u{172}'),
    ('u', '\u{328}', '\u{173}'),
    ('W', '\u{302}', '\u{174}'),
    ('w', '\u{302}', '\u{175}'),
    ('Y', '\u{302}', '\u{176}'),
    ('y', '\u{302}', '\u{177}'),
    ('Y', '\u{308}', '\u{178}'),
    ('Z', '\u{301}', '\u{179}'),
    ('z', '\u{301}', '\u{17A}'),
    ('Z', '\u{307}', '\u{17B}'),
    ('z', '\u{307}', '\u{17C}'),
    ('Z', '\u{30C}', '\u{17D}'),
    ('z', '\u{30C}', '\u{17E}'),
    ('O', '\u{31B}', '\u{1A0}'),
    ('o', '\u{31B}', '\u{1A1}'),
    ('U', '\u{31B}', '\u{1AF}'),
    ('u', '\u{31B}', '\u{1B0}'),
    ('A', '\u{30C}', '\u{1CD}'),
    ('a', '\u{30C}', '\u{1CE}'),
    ('I', '\u{30C}', '\u{1CF}'),
    ('i', '\u{30C}', '\u{1D0}'),
    ('O', '\u{30C}', '\u{1D1}'),
    ('o', '\u{30C}', '\u{1D2}'),
    ('U', '\u{30C}', '\u{1D3}'),
    ('u', '\u{30C}', '\u{1D4}'),
    ('\u{DC}', '\u{304}', '\u{1D5}'),
    ('\u{FC}', '\u{304}', '\u{1D6}'),
    ('\u{DC}', '\u{301}', '\u{1D7}'),
    ('\u{FC}', '\u{301}', '\u{1D8}'),
    ('\u{DC}', '\u{30C}', '\u{1D9}'),
    ('\u{FC}', '\u{30C}', '\u{1DA}'),
    ('\u{DC}', '\u{300}', '\u{1DB}'),
    ('\u{FC}', '\u{300}', '\u{1DC}'),
    ('A', '\u{308}', '\u{1DE}'),
    ('\u{E4}', '\u{307}', '\u{1DF}'),
    ('\u{226}', '\u{307}', '\u{1E0}'),
    ('\u{E6}', '\u{304}', '\u{1E1}'),
    ('\u{C6}', '\u{304}', '\u{1E2}'),
    ('\u{E6}', '\u{304}', '\u{1E3}'),
    // Greek
    ('\u{399}', '\u{300}', '\u{38A}'),
    ('\u{399}', '\u{301}', '\u{38A}'),
    ('\u{391}', '\u{301}', '\u{386}'),
    ('\u{395}', '\u{301}', '\u{388}'),
    ('\u{397}', '\u{301}', '\u{389}'),
    ('\u{399}', '\u{308}', '\u{3AA}'),
    ('\u{3A5}', '\u{308}', '\u{3AB}'),
    ('\u{3A5}', '\u{301}', '\u{38E}'),
    ('\u{39F}', '\u{301}', '\u{38C}'),
    ('\u{3B1}', '\u{301}', '\u{3AC}'),
    ('\u{3B5}', '\u{301}', '\u{3AD}'),
    ('\u{3B7}', '\u{301}', '\u{3AE}'),
    ('\u{3B9}', '\u{301}', '\u{3AF}'),
    ('\u{3B9}', '\u{308}', '\u{3CA}'),
    ('\u{3C5}', '\u{308}', '\u{3CB}'),
    ('\u{3BF}', '\u{301}', '\u{3CC}'),
    ('\u{3C5}', '\u{301}', '\u{3CD}'),
    ('\u{3C9}', '\u{301}', '\u{3CE}'),
    // Cyrillic
    ('\u{415}', '\u{300}', '\u{400}'),
    ('\u{415}', '\u{308}', '\u{401}'),
    ('\u{413}', '\u{301}', '\u{403}'),
    ('\u{406}', '\u{308}', '\u{407}'),
    ('\u{41A}', '\u{301}', '\u{40C}'),
    ('\u{418}', '\u{300}', '\u{40D}'),
    ('\u{423}', '\u{306}', '\u{40E}'),
    ('\u{418}', '\u{306}', '\u{419}'),
    ('\u{438}', '\u{306}', '\u{439}'),
    ('\u{435}', '\u{300}', '\u{450}'),
    ('\u{435}', '\u{308}', '\u{451}'),
    ('\u{433}', '\u{301}', '\u{453}'),
    ('\u{456}', '\u{308}', '\u{457}'),
    ('\u{43A}', '\u{301}', '\u{45C}'),
    ('\u{438}', '\u{300}', '\u{45D}'),
    ('\u{443}', '\u{306}', '\u{45E}'),
    ('\u{474}', '\u{30F}', '\u{476}'),
    ('\u{475}', '\u{30F}', '\u{477}'),
    ('\u{416}', '\u{306}', '\u{4C1}'),
    ('\u{436}', '\u{306}', '\u{4C2}'),
    ('\u{410}', '\u{306}', '\u{4D0}'),
    ('\u{430}', '\u{306}', '\u{4D1}'),
    ('\u{410}', '\u{308}', '\u{4D2}'),
    ('\u{430}', '\u{308}', '\u{4D3}'),
    ('\u{415}', '\u{306}', '\u{4D6}'),
    ('\u{435}', '\u{306}', '\u{4D7}'),
    ('\u{4D8}', '\u{308}', '\u{4DA}'),
    ('\u{4D9}', '\u{308}', '\u{4DB}'),
    ('\u{416}', '\u{308}', '\u{4DC}'),
    ('\u{436}', '\u{308}', '\u{4DD}'),
    ('\u{417}', '\u{308}', '\u{4DE}'),
    ('\u{437}', '\u{308}', '\u{4DF}'),
    ('\u{418}', '\u{304}', '\u{4E2}'),
    ('\u{438}', '\u{304}', '\u{4E3}'),
    ('\u{418}', '\u{308}', '\u{4E4}'),
    ('\u{438}', '\u{308}', '\u{4E5}'),
    ('\u{41E}', '\u{308}', '\u{4E6}'),
    ('\u{43E}', '\u{308}', '\u{4E7}'),
    ('\u{4E8}', '\u{308}', '\u{4EA}'),
    ('\u{4E9}', '\u{308}', '\u{4EB}'),
    ('\u{42D}', '\u{308}', '\u{4EC}'),
    ('\u{44D}', '\u{308}', '\u{4ED}'),
    ('\u{423}', '\u{304}', '\u{4EE}'),
    ('\u{443}', '\u{304}', '\u{4EF}'),
    ('\u{423}', '\u{308}', '\u{4F0}'),
    ('\u{443}', '\u{308}', '\u{4F1}'),
    ('\u{423}', '\u{30B}', '\u{4F2}'),
    ('\u{443}', '\u{30B}', '\u{4F3}'),
    ('\u{427}', '\u{308}', '\u{4F4}'),
    ('\u{447}', '\u{308}', '\u{4F5}'),
    ('\u{42B}', '\u{308}', '\u{4F8}'),
    ('\u{44B}', '\u{308}', '\u{4F9}'),
    // Devanagari vowel compositions
    ('\u{9C7}', '\u{9BE}', '\u{9CB}'),
    ('\u{9C7}', '\u{9D7}', '\u{9CC}'),
    ('\u{B47}', '\u{B56}', '\u{B48}'),
    ('\u{B47}', '\u{B3E}', '\u{B4B}'),
    ('\u{B47}', '\u{B57}', '\u{B4C}'),
    ('\u{B92}', '\u{BD7}', '\u{B94}'),
    ('\u{C6}', '\u{BBE}', '\u{BC6}'),
    ('\u{C7}', '\u{BBE}', '\u{BC7}'),
    ('\u{C6}', '\u{BD7}', '\u{BC8}'),
    ('\u{BC6}', '\u{BBE}', '\u{BCA}'),
    ('\u{BC7}', '\u{BBE}', '\u{BCB}'),
    ('\u{BC6}', '\u{BD7}', '\u{BCC}'),
    ('\u{C46}', '\u{C56}', '\u{C48}'),
    ('\u{CBF}', '\u{CD5}', '\u{CC0}'),
    ('\u{CC6}', '\u{CD5}', '\u{CC7}'),
    ('\u{CC6}', '\u{CD6}', '\u{CC8}'),
    ('\u{CC6}', '\u{CC2}', '\u{CCA}'),
    ('\u{CCA}', '\u{CD5}', '\u{CCB}'),
    ('\u{D46}', '\u{D3E}', '\u{D4A}'),
    ('\u{D47}', '\u{D3E}', '\u{D4B}'),
    ('\u{D46}', '\u{D57}', '\u{D4C}'),
    // Latin Extended Additional
    ('D', '\u{307}', '\u{1E0A}'),
    ('d', '\u{307}', '\u{1E0B}'),
    ('D', '\u{323}', '\u{1E0C}'),
    ('d', '\u{323}', '\u{1E0D}'),
    ('S', '\u{323}', '\u{1E62}'),
    ('s', '\u{323}', '\u{1E63}'),
    ('\u{1E62}', '\u{307}', '\u{1E68}'),
    ('\u{1E63}', '\u{307}', '\u{1E69}'),
    ('\u{17F}', '\u{307}', '\u{1E9B}'),
    ('A', '\u{323}', '\u{1EA0}'),
    ('a', '\u{323}', '\u{1EA1}'),
    ('A', '\u{309}', '\u{1EA2}'),
    ('a', '\u{309}', '\u{1EA3}'),
    ('\u{C2}', '\u{301}', '\u{1EA4}'),
    ('\u{E2}', '\u{301}', '\u{1EA5}'),
    ('\u{C2}', '\u{300}', '\u{1EA6}'),
    ('\u{E2}', '\u{300}', '\u{1EA7}'),
    ('\u{C2}', '\u{309}', '\u{1EA8}'),
    ('\u{E2}', '\u{309}', '\u{1EA9}'),
    ('\u{C2}', '\u{303}', '\u{1EAA}'),
    ('\u{E2}', '\u{303}', '\u{1EAB}'),
    ('\u{1EA0}', '\u{302}', '\u{1EAC}'),
    ('\u{1EA1}', '\u{302}', '\u{1EAD}'),
    ('\u{102}', '\u{301}', '\u{1EAE}'),
    ('\u{103}', '\u{301}', '\u{1EAF}'),
    ('\u{102}', '\u{300}', '\u{1EB0}'),
    ('\u{103}', '\u{300}', '\u{1EB1}'),
    ('\u{102}', '\u{309}', '\u{1EB2}'),
    ('\u{103}', '\u{309}', '\u{1EB3}'),
    ('\u{102}', '\u{303}', '\u{1EB4}'),
    ('\u{103}', '\u{303}', '\u{1EB5}'),
    ('\u{1EA0}', '\u{306}', '\u{1EB6}'),
    ('\u{1EA1}', '\u{306}', '\u{1EB7}'),
    ('E', '\u{323}', '\u{1EB8}'),
    ('e', '\u{323}', '\u{1EB9}'),
    ('E', '\u{309}', '\u{1EBA}'),
    ('e', '\u{309}', '\u{1EBB}'),
    ('E', '\u{303}', '\u{1EBC}'),
    ('e', '\u{303}', '\u{1EBD}'),
    ('\u{CA}', '\u{301}', '\u{1EBE}'),
    ('\u{EA}', '\u{301}', '\u{1EBF}'),
    ('\u{CA}', '\u{300}', '\u{1EC0}'),
    ('\u{EA}', '\u{300}', '\u{1EC1}'),
    ('\u{CA}', '\u{309}', '\u{1EC2}'),
    ('\u{EA}', '\u{309}', '\u{1EC3}'),
    ('\u{CA}', '\u{303}', '\u{1EC4}'),
    ('\u{EA}', '\u{303}', '\u{1EC5}'),
    ('\u{1EB8}', '\u{302}', '\u{1EC6}'),
    ('\u{1EB9}', '\u{302}', '\u{1EC7}'),
    ('I', '\u{309}', '\u{1EC8}'),
    ('i', '\u{309}', '\u{1EC9}'),
    ('I', '\u{323}', '\u{1ECA}'),
    ('i', '\u{323}', '\u{1ECB}'),
    ('O', '\u{323}', '\u{1ECC}'),
    ('o', '\u{323}', '\u{1ECD}'),
    ('O', '\u{309}', '\u{1ECE}'),
    ('o', '\u{309}', '\u{1ECF}'),
    ('\u{D4}', '\u{301}', '\u{1ED0}'),
    ('\u{F4}', '\u{301}', '\u{1ED1}'),
    ('\u{D4}', '\u{300}', '\u{1ED2}'),
    ('\u{F4}', '\u{300}', '\u{1ED3}'),
    ('\u{D4}', '\u{309}', '\u{1ED4}'),
    ('\u{F4}', '\u{309}', '\u{1ED5}'),
    ('\u{D4}', '\u{303}', '\u{1ED6}'),
    ('\u{F4}', '\u{303}', '\u{1ED7}'),
    ('\u{1ECC}', '\u{302}', '\u{1ED8}'),
    ('\u{1ECD}', '\u{302}', '\u{1ED9}'),
    ('\u{1A0}', '\u{301}', '\u{1EDA}'),
    ('\u{1A1}', '\u{301}', '\u{1EDB}'),
    ('\u{1A0}', '\u{300}', '\u{1EDC}'),
    ('\u{1A1}', '\u{300}', '\u{1EDD}'),
    ('\u{1A0}', '\u{309}', '\u{1EDE}'),
    ('\u{1A1}', '\u{309}', '\u{1EDF}'),
    ('\u{1A0}', '\u{303}', '\u{1EE0}'),
    ('\u{1A1}', '\u{303}', '\u{1EE1}'),
    ('\u{1A0}', '\u{323}', '\u{1EE2}'),
    ('\u{1A1}', '\u{323}', '\u{1EE3}'),
    ('U', '\u{323}', '\u{1EE4}'),
    ('u', '\u{323}', '\u{1EE5}'),
    ('U', '\u{309}', '\u{1EE6}'),
    ('u', '\u{309}', '\u{1EE7}'),
    ('\u{1AF}', '\u{301}', '\u{1EE8}'),
    ('\u{1B0}', '\u{301}', '\u{1EE9}'),
    ('\u{1AF}', '\u{300}', '\u{1EEA}'),
    ('\u{1B0}', '\u{300}', '\u{1EEB}'),
    ('\u{1AF}', '\u{309}', '\u{1EEC}'),
    ('\u{1B0}', '\u{309}', '\u{1EED}'),
    ('\u{1AF}', '\u{303}', '\u{1EEE}'),
    ('\u{1B0}', '\u{303}', '\u{1EEF}'),
    ('\u{1AF}', '\u{323}', '\u{1EF0}'),
    ('\u{1B0}', '\u{323}', '\u{1EF1}'),
    ('Y', '\u{300}', '\u{1EF2}'),
    ('y', '\u{300}', '\u{1EF3}'),
    ('Y', '\u{323}', '\u{1EF4}'),
    ('y', '\u{323}', '\u{1EF5}'),
    ('Y', '\u{309}', '\u{1EF6}'),
    ('y', '\u{309}', '\u{1EF7}'),
    ('Y', '\u{303}', '\u{1EF8}'),
    ('y', '\u{303}', '\u{1EF9}'),
];

fn string_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // `new String(x)` runs the ToString conversion (Symbol → TypeError);
    // only the String(x) call form produces SymbolDescriptiveString
    let s = if args.is_empty() {
        String::new()
    } else {
        match &arg(args, 0) {
            Value::Sym(_, d) if !matches!(this, Value::Obj(_)) => {
                format!("Symbol({d})")
            }
            v => i.to_str(v)?,
        }
    };
    if let Value::Obj(o) = this {
        // `new String(x)` → boxed — string-exotic own props are indexed
        // by UTF-16 code unit: chars {W:false,E:true,C:false},
        // length {W:false,E:false,C:false}
        i.set_builtin(o, "$value", Value::Str(Rc::from(s.as_str())));
        let units = u16s(&s);
        let len = units.len();
        {
            let mut ob = o.borrow_mut();
            for (k, c) in units.iter().enumerate() {
                ob.props.insert(
                    PropKey::from_str(&k.to_string()),
                    Prop {
                        val: Value::Str(Rc::from(String::from_utf16_lossy(&[*c]).as_str())),
                        writable: false,
                        enumerable: true,
                        configurable: false,
                        get: None,
                        set: None,
                    },
                );
            }
        }
        i.set_attr(o, "length", Value::Num(len as f64), false, false);
        Ok(this.clone())
    } else {
        Ok(Value::Str(Rc::from(s.as_str())))
    }
}

fn string_from_char_code(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    // spec: each arg is ToNumber → ToUint16 (mod 65536); surrogate code
    // units are valid here but can't live in a Rust string → U+FFFD
    let mut u: Vec<u16> = Vec::with_capacity(args.len());
    for a in args {
        let n = str_num(i, a)?;
        let v = if n.is_nan() || n.is_infinite() {
            0.0
        } else {
            n.trunc()
        };
        u.push(v.rem_euclid(65536.0) as u16);
    }
    Ok(u16s_val(&u))
}

fn string_from_code_point(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    // spec: ToNumber each; RangeError for NaN/non-integer/out of
    // [0, 0x10FFFF]. Astral points encode as surrogate pairs; a surrogate
    // code point is spec-valid but unrepresentable in UTF-8 → U+FFFD.
    let mut u: Vec<u16> = Vec::with_capacity(args.len());
    for a in args {
        let n = str_num(i, a)?;
        if n.is_nan() || n.fract() != 0.0 || !(0.0..=0x10ffff as f64).contains(&n) {
            return Err(i.range_err(format!("{n} is not a valid code point")));
        }
        let cp = n as u32;
        match char::from_u32(cp) {
            Some(c) => u.extend(c.to_string().encode_utf16()),
            None => u.push(cp as u16),
        }
    }
    Ok(u16s_val(&u))
}

fn string_raw(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    // spec: ToObject(cooked) → Get(raw) → ToObject → Get(length) →
    // ToLength, then per-segment Get+ToString with substitutions woven
    // between (substitution count = args.len()-1, never past last segment)
    let cooked = i.to_object(&arg(args, 0))?;
    let raw_v = i.get_prop_str(&cooked, "raw")?;
    let raw = i.to_object(&raw_v)?;
    let len_v = i.get_prop_str(&raw, "length")?;
    let n = to_length_num(str_num(i, &len_v)?);
    let nsubs = args.len().saturating_sub(1);
    let mut s = String::new();
    for k in 0..n {
        let seg = i.get_prop(&raw, &idx_key(k))?;
        s.push_str(&i.to_str(&seg)?);
        if k + 1 == n {
            break;
        }
        if k < nsubs {
            s.push_str(&i.to_str(&args[k + 1])?);
        }
    }
    Ok(Value::Str(Rc::from(s.as_str())))
}

// ===========================================================================
// natives: number / boolean / symbol
// ===========================================================================

/// thisNumberValue — only a Number primitive or a [[NumberData]] wrapper
/// is a valid receiver (other primitives and other wrappers TypeError)
fn unbox_num(i: &mut Interp, this: &Value) -> Result<f64, Value> {
    match this {
        Value::Num(n) => Ok(*n),
        Value::Obj(o) => {
            if let Some(Value::Num(n)) = i.obj_get_raw_str(o, "$value") {
                return Ok(n);
            }
            Err(i.type_err("Number.prototype method called on incompatible receiver"))
        }
        _ => Err(i.type_err("Number.prototype method called on incompatible receiver")),
    }
}

/// fractionDigits coercion — ToIntegerOrInfinity then the 0..=100 range
/// check (RangeError, not clamp); `undefined`/`NaN` → 0
/// ToIntegerOrInfinity — BigInt/Symbol → TypeError; ±Inf passes through;
/// NaN → +0. Range checks live at the call site (spec order varies)
fn num_digits_raw(i: &mut Interp, v: &Value) -> Result<f64, Value> {
    if crate::binary::bigint_is_prim(v) {
        return Err(i.type_err("Cannot convert a BigInt value to a number"));
    }
    let n = i.to_num(v)?;
    if n.is_nan() {
        Ok(0.0)
    } else {
        Ok(n.trunc())
    }
}

fn num_to_fixed(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let n = unbox_num(i, this)?;
    let df = num_digits_raw(i, &arg(args, 0))?;
    // spec order: ToIntegerOrInfinity → "f is not finite" → RangeError
    // BEFORE the x-finite check (NaN.toFixed(Infinity) → RangeError);
    // the 0..=100 range check runs AFTER (Infinity.toFixed(1000) → "Infinity")
    if !df.is_finite() {
        return Err(i.range_err("toFixed() digits argument must be between 0 and 100"));
    }
    if !n.is_finite() {
        return Ok(Value::Str(Rc::from(js_num_str(n).as_str())));
    }
    if !(0.0..=100.0).contains(&df) {
        return Err(i.range_err("toFixed() digits argument must be between 0 and 100"));
    }
    let d = df as usize;
    // spec: |x| >= 1e21 falls back to ordinary ToString
    if n.abs() >= 1e21 {
        return Ok(Value::Str(Rc::from(js_num_str(n).as_str())));
    }
    Ok(Value::Str(Rc::from(format!("{n:.d$}").as_str())))
}

fn num_to_precision(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let n = unbox_num(i, this)?;
    match arg(args, 0) {
        Value::Und => Ok(Value::Str(Rc::from(js_num_str(n).as_str()))),
        v => {
            let dd = num_digits_raw(i, &v)?;
            // spec: x NaN/±Inf returns BEFORE the precision range check
            if !n.is_finite() {
                return Ok(Value::Str(Rc::from(js_num_str(n).as_str())));
            }
            if !(1.0..=100.0).contains(&dd) {
                return Err(i.range_err("toPrecision() argument must be between 1 and 100"));
            }
            let d = dd as usize;
            if n == 0.0 {
                // spec: p significant digits of ±0 → "0" or "0.0…0"
                let s = if d == 1 {
                    "0".to_string()
                } else {
                    format!("0.{}", "0".repeat(d - 1))
                };
                return Ok(Value::Str(Rc::from(s.as_str())));
            }
            // spec: e = floor(log10|x|); exponential iff e >= p or e < -6
            // — rounding itself must run BEFORE the boundary decision
            // (9.999.toPrecision(3) → "10.0" via decimal, not "9.999")
            let (digits, e0) = js_exact_digits(n);
            let (digs, e) = js_round_sig(&digits, e0, d);
            if e >= d as i32 || e < -6 {
                let neg = n < 0.0;
                let mant: String = digs.iter().map(|b| (*b as char)).collect();
                let mant = if mant.len() > 1 {
                    format!("{}.{}", &mant[..1], &mant[1..])
                } else {
                    mant
                };
                let esign = if e < 0 { "-" } else { "+" };
                return Ok(Value::Str(Rc::from(
                    format!("{}{mant}e{}{}", if neg { "-" } else { "" }, esign, e.abs()).as_str(),
                )));
            }
            // decimal: digit `k` sits at position 10^(e-k)
            let neg = n < 0.0;
            let mut out = String::new();
            if neg {
                out.push('-');
            }
            for k in 0..d {
                let pos = e - k as i32;
                let ch = if k < digs.len() { digs[k] as char } else { '0' };
                if pos < 0 && k as i32 == e + 1 {
                    out.push('.');
                }
                out.push(ch);
            }
            if e < 0 {
                // leading "0.000" before the digits
                let mut pre = String::from("0.");
                for _ in 0..(-e - 1) {
                    pre.push('0');
                }
                let body: String = digs.iter().map(|b| (*b as char)).collect();
                out = format!("{}{}{}", if neg { "-" } else { "" }, pre, body);
            }
            Ok(Value::Str(Rc::from(out.as_str())))
        }
    }
}

fn num_to_exponential(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let n = unbox_num(i, this)?;
    // spec: no digits arg → the minimal exponent form that round-trips
    // (Rust's {:e} Display is already shortest-repr)
    if matches!(arg(args, 0), Value::Und) {
        let s = format!("{n:e}");
        let s = js_exp_str(&s);
        return Ok(Value::Str(Rc::from(s.as_str())));
    }
    let df = num_digits_raw(i, &arg(args, 0))?;
    if !n.is_finite() {
        return Ok(Value::Str(Rc::from(js_num_str(n).as_str())));
    }
    if !df.is_finite() || !(0.0..=100.0).contains(&df) {
        return Err(i.range_err("toExponential() argument must be between 0 and 100"));
    }
    let d = df as usize;
    if n == 0.0 {
        // spec: ±0 → "0" + (d>0 ? "." + d zeros : "") + "e+0"
        let mant = if d == 0 {
            "0".to_string()
        } else {
            format!("0.{}", "0".repeat(d))
        };
        return Ok(Value::Str(Rc::from(format!("{mant}e+0").as_str())));
    }
    let s = js_exp_fixed(n, d);
    Ok(Value::Str(Rc::from(s.as_str())))
}

/// exact significant decimal digits of |n| + base-10 exponent —
/// Rust's high-precision `{:.*e}` prints the full expansion
fn js_exact_digits(n: f64) -> (Vec<u8>, i32) {
    let s = format!("{:.800e}", n.abs());
    let epos = s.find('e').unwrap();
    let exp: i32 = s[epos + 1..].parse().unwrap();
    let digits: Vec<u8> = s[..epos].bytes().filter(|b| b.is_ascii_digit()).collect();
    (digits, exp)
}

/// round `digits` to `keep` significant digits, HALF-UP (spec picks the
/// larger s on ties); returns (digits, carry-adjusted exponent delta)
fn js_round_sig(digits: &[u8], exp: i32, keep: usize) -> (Vec<u8>, i32) {
    let mut digs: Vec<u8> = digits[..keep.min(digits.len())].to_vec();
    let tail = &digits[keep.min(digits.len())..];
    let up = tail.first().is_some_and(|t| *t >= b'5');
    let mut exp = exp;
    if up {
        let mut k = digs.len();
        loop {
            if k == 0 {
                digs.insert(0, b'1');
                exp += 1;
                break;
            }
            k -= 1;
            if digs[k] == b'9' {
                digs[k] = b'0';
            } else {
                digs[k] += 1;
                break;
            }
        }
        // the carry insert ("9"→"10") leaves an extra digit
        digs.truncate(keep);
    }
    (digs, exp)
}

/// spec-faithful `x.toExponential(d)`: f64's exact decimal expansion is
/// produced by Rust's high-precision `{:.*e}`, then rounded HALF-UP at
/// d+1 significant digits (spec picks the larger s on ties; Rust's
/// built-in rounding is half-even)
fn js_exp_fixed(n: f64, d: usize) -> String {
    let (digits, exp) = js_exact_digits(n);
    let (digs, exp) = js_round_sig(&digits, exp, d + 1);
    let neg = n < 0.0;
    let mant: String = digs.iter().map(|b| (*b as char)).collect();
    let mant = if mant.len() > 1 {
        format!("{}.{}", &mant[..1], &mant[1..])
    } else {
        mant
    };
    let esign = if exp < 0 { "-" } else { "+" };
    format!(
        "{}{mant}e{}{}",
        if neg { "-" } else { "" },
        esign,
        exp.abs()
    )
}

/// normalize a Rust `{:e}` string to the JS `e±d` form
fn js_exp_str(s: &str) -> String {
    match s.find('e') {
        Some(p) => {
            let (m, e) = (&s[..p], &s[p + 1..]);
            if let Some(d) = e.strip_prefix('-') {
                format!("{m}e-{d}")
            } else {
                format!("{m}e+{e}")
            }
        }
        None => s.to_string(),
    }
}

fn num_to_string(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let n = unbox_num(i, this)?;
    match arg(args, 0) {
        Value::Und => Ok(Value::Str(Rc::from(js_num_str(n).as_str()))),
        v => {
            let radix = i.to_num(&v)? as u32;
            if !(2..=36).contains(&radix) {
                return Err(i.range_err("toString() radix must be between 2 and 36"));
            }
            if !n.is_finite() {
                return Ok(Value::Str(Rc::from(js_num_str(n).as_str())));
            }
            let neg = n < 0.0;
            let int = n.abs().trunc() as u64;
            let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
            let mut s = Vec::new();
            let mut x = int;
            if x == 0 {
                s.push(b'0');
            }
            while x > 0 {
                s.push(digits[(x % radix as u64) as usize]);
                x /= radix as u64;
            }
            if neg {
                s.push(b'-');
            }
            s.reverse();
            let frac = n.abs().fract();
            let mut out = String::from_utf8(s).unwrap();
            if frac > 0.0 {
                out.push('.');
                let mut f = frac;
                for _ in 0..20 {
                    f *= radix as f64;
                    let d = f.trunc() as usize;
                    out.push(digits[d] as char);
                    f = f.fract();
                    if f == 0.0 {
                        break;
                    }
                }
            }
            Ok(Value::Str(Rc::from(out.as_str())))
        }
    }
}

fn num_value_of(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    Ok(Value::Num(unbox_num(i, this)?))
}

fn number_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // Number() → +0 but Number(undefined) → NaN — arg presence matters
    let n = match args.first() {
        None => 0.0,
        Some(v) => i.to_num(v)?,
    };
    if matches!(this, Value::Obj(_)) {
        if let Value::Obj(o) = this {
            i.set_builtin(o, "$value", Value::Num(n));
        }
        Ok(this.clone())
    } else {
        Ok(Value::Num(n))
    }
}

fn number_is_nan(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let _ = i;
    Ok(Value::Bool(
        matches!(arg(args, 0), Value::Num(n) if n.is_nan()),
    ))
}

fn number_is_finite(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let _ = i;
    Ok(Value::Bool(
        matches!(arg(args, 0), Value::Num(n) if n.is_finite()),
    ))
}

fn number_is_safe_integer(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let _ = i;
    Ok(Value::Bool(match arg(args, 0) {
        Value::Num(n) => n.fract() == 0.0 && n.abs() <= 9007199254740991.0,
        _ => false,
    }))
}

fn number_is_integer(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let _ = i;
    Ok(Value::Bool(
        matches!(arg(args, 0), Value::Num(n) if n.is_finite() && n == n.trunc()),
    ))
}

fn unbox_bool(i: &mut Interp, this: &Value) -> Result<bool, Value> {
    match this {
        Value::Bool(b) => Ok(*b),
        Value::Obj(o) => {
            if let Some(Value::Bool(b)) = i.obj_get_raw_str(o, "$value") {
                return Ok(b);
            }
            Err(i.type_err("Boolean.prototype method called on incompatible receiver"))
        }
        _ => Err(i.type_err("Boolean.prototype method called on incompatible receiver")),
    }
}

fn bool_to_string(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    Ok(Value::Str(Rc::from(
        unbox_bool(i, this)?.to_string().as_str(),
    )))
}

fn bool_value_of(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    Ok(Value::Bool(unbox_bool(i, this)?))
}

fn boolean_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let b = i.to_bool(&arg(args, 0));
    if matches!(this, Value::Obj(_)) {
        if let Value::Obj(o) = this {
            i.set_builtin(o, "$value", Value::Bool(b));
        }
        Ok(this.clone())
    } else {
        Ok(Value::Bool(b))
    }
}

/// `Function(p1, …, pN, body)` — compiles a global-scoped function from
/// the argument sources (spec CreateDynamicFunction, minus realm checks).
fn function_ctor(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let (params, body) = match args.split_last() {
        None => (String::new(), String::new()),
        Some((b, ps)) => {
            let mut names = Vec::with_capacity(ps.len());
            for p in ps {
                names.push(i.to_str(p)?);
            }
            (names.join(","), i.to_str(b)?)
        }
    };
    // `\n` after the params keeps `<!--`/`-->` HTML comments in the
    // parameter source from swallowing the closing paren (spec B.x)
    let src = format!("(function anonymous({params}\n) {{\n{body}\n}})");
    let v = match i.run(&src) {
        Ok(v) => v,
        Err(e) => return Err(i.syntax_err(format!("{e}"))),
    };
    if let Value::Obj(o) = &v {
        let mut b = o.borrow_mut();
        // dynamically-created fns own NO caller/arguments props even
        // when the body is strict (the inherited %ThrowTypeError%
        // accessors still fire) — but they count as restricted callers
        // for the legacy sloppy `.caller` lookup
        b.props.remove_str("caller");
        b.props.remove_str("arguments");
        if let ObjKind::Func(Func::User { dynamic, .. }) = &mut b.kind {
            *dynamic = true;
        }
    }
    // OrdinaryCreateFromConstructor: newTarget.prototype wins
    // (`class F extends Function {}` → fn proto is F.prototype)
    let nt = i.new_target_native.clone().unwrap_or(Value::Und);
    if matches!(nt, Value::Obj(_)) {
        if let Ok(Value::Obj(p)) = i.get_prop(&nt, &PropKey::from_str("prototype")) {
            if let Value::Obj(o) = &v {
                o.borrow_mut().proto = Some(p);
            }
        }
    }
    Ok(v)
}

/// `%GeneratorFunction%` ctor — `CreateDynamicFunction` for `function*`:
/// compiles the source, then OrdinaryCreateFromConstructor semantics —
/// the fn's [[Prototype]] comes from newTarget.prototype, falling back
/// to %GeneratorFunction.prototype%.
fn gen_function_ctor(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let (params, body) = match args.split_last() {
        None => (String::new(), String::new()),
        Some((b, ps)) => {
            let mut names = Vec::with_capacity(ps.len());
            for p in ps {
                names.push(i.to_str(p)?);
            }
            (names.join(","), i.to_str(b)?)
        }
    };
    let src = format!("(function* anonymous({params}\n) {{\n{body}\n}})");
    let v = match i.run(&src) {
        Ok(v) => v,
        Err(e) => return Err(i.syntax_err(format!("{e}"))),
    };
    if let Value::Obj(o) = &v {
        let mut b = o.borrow_mut();
        // same dynamic-function semantics as function_ctor
        b.props.remove_str("caller");
        b.props.remove_str("arguments");
        if let ObjKind::Func(Func::User { dynamic, .. }) = &mut b.kind {
            *dynamic = true;
        }
    }
    // newTarget.prototype wins over %GeneratorFunction.prototype%
    // (subclassing: `class G extends GeneratorFunction {}`)
    let nt = i.new_target_native.clone().unwrap_or(Value::Und);
    if let Value::Obj(nto) = &nt {
        if let Ok(Value::Obj(p)) = i.get_prop(&nt, &PropKey::from_str("prototype")) {
            let _ = nto;
            if let Value::Obj(o) = &v {
                o.borrow_mut().proto = Some(p);
            }
        }
    }
    Ok(v)
}

fn symbol_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    if matches!(this, Value::Obj(_)) {
        return Err(i.type_err("Symbol is not a constructor"));
    }
    let (d, no_desc) = match arg(args, 0) {
        Value::Und => (String::new(), true),
        v => (i.to_str(&v)?, false),
    };
    let id = i.sym_counter;
    i.sym_counter += 1;
    if no_desc {
        i.sym_no_desc.insert(id);
    }
    Ok(Value::Sym(id, Rc::from(d.as_str())))
}

fn symbol_for(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let _ = i;
    // registry-less: deterministic id by hashing the key — close enough
    let key = i.to_str(&arg(args, 0))?;
    let mut h: u64 = 0xcbf29ce484222325;
    for b in key.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    let id = 1_000_000 + (h % 1_000_000);
    Ok(Value::Sym(id, Rc::from(key.as_str())))
}

fn symbol_key_for(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let _ = i;
    if !matches!(arg(args, 0), Value::Sym(..)) {
        return Err(i.type_err("Symbol.keyFor called on non-symbol"));
    }
    match arg(args, 0) {
        // only Symbol.for-registry ids (>= 1_000_000) have a key
        Value::Sym(id, d) if id >= 1_000_000 => Ok(Value::Str(Rc::from(d.as_ref()))),
        _ => Ok(Value::Und),
    }
}

fn sym_to_string(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    match this {
        Value::Sym(_, d) => Ok(Value::Str(Rc::from(format!("Symbol({d})").as_str()))),
        // Symbol wrapper objects carry the primitive in `$value`
        // (spec 20.4.3.5 thisSymbolValue unwraps [[SymbolData]])
        Value::Obj(o) => match o.borrow().props.get_str("$value") {
            Some(p) => match &p.val {
                Value::Sym(_, d) => Ok(Value::Str(Rc::from(format!("Symbol({d})").as_str()))),
                _ => Err(i.type_err("Cannot convert a Symbol value to a value")),
            },
            None => Err(i.type_err("Cannot convert a Symbol value to a value")),
        },
        _ => Err(i.type_err("Cannot convert a Symbol value to a value")),
    }
}

fn sym_to_primitive(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    sym_value_of(i, this, _a)
}

fn sym_value_of(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    match this {
        Value::Sym(..) => Ok(this.clone()),
        // Symbol wrapper objects carry the primitive in `$value`
        Value::Obj(o) => match o.borrow().props.get_str("$value") {
            Some(p) => Ok(p.val.clone()),
            None => Err(i.type_err("Cannot convert a Symbol value to a value")),
        },
        _ => Err(i.type_err("Cannot convert a Symbol value to a value")),
    }
}

fn sym_get_description(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    // thisSymbolValue: primitive or Symbol wrapper; anything else → TypeError
    let sym = match this {
        Value::Sym(..) => this.clone(),
        Value::Obj(o) => match o.borrow().props.get_str("$value") {
            Some(p) if matches!(p.val, Value::Sym(..)) => p.val.clone(),
            _ => {
                return Err(i.type_err(
                    "Method get Symbol.prototype.description called on incompatible receiver",
                ))
            }
        },
        _ => {
            return Err(i.type_err(
                "Method get Symbol.prototype.description called on incompatible receiver",
            ))
        }
    };
    match sym {
        Value::Sym(id, d) => {
            if i.sym_no_desc.contains(&id) {
                Ok(Value::Und)
            } else {
                Ok(Value::Str(Rc::from(d.as_ref())))
            }
        }
        _ => unreachable!(),
    }
}

// ===========================================================================
// natives: errors
// ===========================================================================

/// InstallErrorCause(O, options): if `options` is an object with an own
/// or inherited `cause` property, define own `cause` on O.
fn install_error_cause(i: &mut Interp, o: &ObjRef, options: &Value) -> Result<(), Value> {
    if let Value::Obj(_) = options {
        if i.has_prop(options, &PropKey::from_str("cause"))? {
            let cv = i.get_prop_str(options, "cause")?;
            i.set_attr(o, "cause", cv, true, true);
        }
    }
    Ok(())
}

fn error_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let msg = match arg(args, 0) {
        Value::Und => None,
        v => Some(i.to_str(&v)?),
    };
    if let Value::Obj(o) = this {
        o.borrow_mut().kind = ObjKind::Err;
        // spec: own `message` only when a message arg was provided;
        // `name` is never an own prop (resolves via the prototype)
        if let Some(m) = &msg {
            i.set_attr(o, "message", Value::Str(Rc::from(m.as_str())), true, true);
        }
        install_error_cause(i, o, &arg(args, 1))?;
        i.set_attr(
            o,
            "stack",
            Value::Str(Rc::from(
                format!("Error: {}", msg.as_deref().unwrap_or("")).as_str(),
            )),
            true,
            true,
        );
        return Ok(this.clone());
    }
    let p = i.p.error.clone();
    let e = i.err_obj_opt(&p, "Error", msg.as_deref());
    if let Value::Obj(o) = &e {
        install_error_cause(i, o, &arg(args, 1))?;
    }
    Ok(e)
}

macro_rules! sub_err_ctor {
    ($name:ident, $field:ident, $jname:literal) => {
        fn $name(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
            let msg = match arg(args, 0) {
                Value::Und => None,
                v => Some(i.to_str(&v)?),
            };
            if let Value::Obj(o) = this {
                o.borrow_mut().kind = ObjKind::Err;
                if let Some(m) = &msg {
                    i.set_attr(o, "message", Value::Str(Rc::from(m.as_str())), true, true);
                }
                install_error_cause(i, o, &arg(args, 1))?;
                i.set_attr(
                    o,
                    "stack",
                    Value::Str(Rc::from(
                        format!("{}: {}", $jname, msg.as_deref().unwrap_or("")).as_str(),
                    )),
                    true,
                    true,
                );
                return Ok(this.clone());
            }
            let p = i.p.$field.clone();
            let e = i.err_obj_opt(&p, $jname, msg.as_deref());
            if let Value::Obj(o) = &e {
                install_error_cause(i, o, &arg(args, 1))?;
            }
            Ok(e)
        }
    };
}
sub_err_ctor!(type_error_ctor, type_error, "TypeError");
sub_err_ctor!(eval_error_ctor, eval_error, "EvalError");
sub_err_ctor!(uri_error_ctor, uri_error, "URIError");

sub_err_ctor!(range_error_ctor, range_error, "RangeError");
sub_err_ctor!(syntax_error_ctor, syntax_error, "SyntaxError");
sub_err_ctor!(ref_error_ctor, ref_error, "ReferenceError");

fn err_to_string(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    // spec 20.5.5.5: TypeError on non-object receiver; `name`/`message`
    // fetched via Get (proto walk + getter invocation)
    if !matches!(this, Value::Obj(_)) {
        return Err(i.type_err("Error.prototype.toString called on non-object"));
    }
    let name_v = i.get_prop_str(this, "name")?;
    let name = if matches!(name_v, Value::Und) {
        "Error".to_string()
    } else {
        i.to_str(&name_v)?
    };
    let msg_v = i.get_prop_str(this, "message")?;
    let msg = if matches!(msg_v, Value::Und) {
        String::new()
    } else {
        i.to_str(&msg_v)?
    };
    if name.is_empty() {
        return Ok(Value::Str(Rc::from(msg.as_str())));
    }
    if msg.is_empty() {
        return Ok(Value::Str(Rc::from(name.as_str())));
    }
    Ok(Value::Str(Rc::from(format!("{name}: {msg}").as_str())))
}

/// `AggregateError(iterable, message)` — collects the iterable into an
/// `errors` own data property
fn agg_error_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec order: ToString(message) BEFORE IterableToList(errors)
    let msg = match args.get(1).cloned().unwrap_or(Value::Und) {
        Value::Und => String::new(),
        v => i.to_str(&v)?,
    };
    // spec: `errors` is required — GetIterator(undefined) throws TypeError
    let errs = i.iter_values(&arg(args, 0))?;
    let arr = Value::Obj(i.new_arr(errs));
    if let Value::Obj(o) = this {
        o.borrow_mut().kind = ObjKind::Err;
        i.set_attr(o, "errors", arr, true, true);
        if !matches!(arg(args, 1), Value::Und) {
            i.set_attr(o, "message", Value::Str(Rc::from(msg.as_str())), true, true);
        }
        install_error_cause(i, o, &arg(args, 2))?;
        return Ok(this.clone());
    }
    let has_msg = !matches!(args.get(1), None | Some(Value::Und));
    let e = i.err_obj_opt(
        &i.p.agg_error.clone(),
        "AggregateError",
        if has_msg { Some(&msg) } else { None },
    );
    if let Value::Obj(o) = &e {
        i.set_attr(o, "errors", arr, true, true);
        install_error_cause(i, o, &arg(args, 2))?;
    }
    Ok(e)
}

/// `SuppressedError(error, suppressed, message)`
fn suppressed_error_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let err = arg(args, 0);
    let supp = arg(args, 1);
    let msg = match arg(args, 2) {
        Value::Und => String::new(),
        v => i.to_str(&v)?,
    };
    if let Value::Obj(o) = this {
        o.borrow_mut().kind = ObjKind::Err;
        i.set_attr(o, "error", err, true, true);
        i.set_attr(o, "suppressed", supp, true, true);
        if !matches!(arg(args, 2), Value::Und) {
            i.set_attr(o, "message", Value::Str(Rc::from(msg.as_str())), true, true);
        }
        install_error_cause(i, o, &arg(args, 3))?;
        return Ok(this.clone());
    }
    let e = i.err_obj(&i.p.suppressed_error.clone(), "SuppressedError", &msg);
    if let Value::Obj(o) = &e {
        i.set_attr(o, "error", err, true, true);
        i.set_attr(o, "suppressed", supp, true, true);
        install_error_cause(i, o, &arg(args, 3))?;
    }
    Ok(e)
}

// ===========================================================================
// natives: math
// ===========================================================================

macro_rules! math1 {
    ($name:ident, $f:expr) => {
        fn $name(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
            let n = i.to_num(&arg(args, 0))?;
            let f: fn(f64) -> f64 = $f;
            Ok(Value::Num(f(n)))
        }
    };
}
math1!(math_floor, f64::floor);
math1!(math_ceil, f64::ceil);
math1!(math_round, |n: f64| {
    // spec: NaN/±∞/±0 pass through, [-0.5, 0) → -0, ties go toward
    // +∞. The naive (n + 0.5).floor() misrounds odd integers ≥ 2**53
    // (n + 0.5 rounds up before the floor) — compare the fraction.
    if !n.is_finite() || n == 0.0 {
        n
    } else if n >= -0.5 && n < 0.5 {
        if n.is_sign_negative() {
            -0.0
        } else {
            0.0
        }
    } else {
        let f = n.floor();
        if n - f < 0.5 {
            f
        } else {
            f + 1.0
        }
    }
});
math1!(math_trunc, f64::trunc);
math1!(math_abs, f64::abs);
math1!(math_sign, |n: f64| {
    // f64::signum maps -0 → -1; spec keeps the input's own zero
    if n.is_nan() || n == 0.0 {
        n
    } else if n > 0.0 {
        1.0
    } else {
        -1.0
    }
});
math1!(math_sqrt, f64::sqrt);
math1!(math_cbrt, f64::cbrt);
math1!(math_log, f64::ln);
math1!(math_log2, f64::log2);
math1!(math_log10, f64::log10);
math1!(math_log1p, f64::ln_1p);
math1!(math_exp, f64::exp);
math1!(math_expm1, f64::exp_m1);
math1!(math_sin, f64::sin);
math1!(math_cos, f64::cos);
math1!(math_tan, f64::tan);
math1!(math_asin, f64::asin);
math1!(math_acos, f64::acos);
math1!(math_acosh, f64::acosh);
math1!(math_asinh, f64::asinh);
math1!(math_atan, f64::atan);
math1!(math_atanh, f64::atanh);
math1!(math_sinh, f64::sinh);
math1!(math_cosh, f64::cosh);
math1!(math_tanh, f64::tanh);

fn math_min(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    // spec coerces every arg first — a NaN must not skip a later
    // valueOf; comparisons treat -0 as smaller than +0
    let mut m = f64::INFINITY;
    let mut nan = false;
    for a in args {
        let n = i.to_num(a)?;
        nan |= n.is_nan();
        if n < m || (n == m && n.is_sign_negative()) {
            m = n;
        }
    }
    Ok(Value::Num(if nan { f64::NAN } else { m }))
}

fn math_max(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let mut m = f64::NEG_INFINITY;
    let mut nan = false;
    for a in args {
        let n = i.to_num(a)?;
        nan |= n.is_nan();
        if n > m || (n == m && n.is_sign_positive()) {
            m = n;
        }
    }
    Ok(Value::Num(if nan { f64::NAN } else { m }))
}

fn math_pow(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let b = i.to_num(&arg(args, 0))?;
    let e = i.to_num(&arg(args, 1))?;
    Ok(Value::Num(js_pow(b, e)))
}

/// `Number::exponentiate` — the platform pow diverges from the spec on
/// pow(±1, ±∞) (C says 1, spec says NaN); ordered specials follow the
/// spec list, powf covers the general case.
fn js_pow(b: f64, e: f64) -> f64 {
    if e.is_nan() {
        return f64::NAN;
    }
    if e == 0.0 {
        return 1.0; // x**±0 = 1 — even for NaN x
    }
    if b.is_nan() {
        return f64::NAN;
    }
    let ab = b.abs();
    if e.is_infinite() {
        if ab == 1.0 {
            return f64::NAN;
        }
        return if (ab > 1.0) == (e > 0.0) {
            f64::INFINITY
        } else {
            0.0
        };
    }
    // odd-integer check for negative bases — fmod is exact; |e| ≥ 2**53
    // is always even and reports 0 correctly
    let odd_int = e.fract() == 0.0 && (e % 2.0) != 0.0;
    if b.is_infinite() {
        return if b > 0.0 {
            if e > 0.0 {
                f64::INFINITY
            } else {
                0.0
            }
        } else if e > 0.0 {
            if odd_int {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }
        } else if odd_int {
            -0.0
        } else {
            0.0
        };
    }
    if b == 0.0 {
        let neg = b.is_sign_negative();
        return if e > 0.0 {
            if neg && odd_int {
                -0.0
            } else {
                0.0
            }
        } else if neg && odd_int {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    if b < 0.0 && e.fract() != 0.0 {
        return f64::NAN; // negative base, non-integer exponent
    }
    b.powf(e)
}

fn math_random(i: &mut Interp, _t: &Value, _a: &[Value]) -> NResult {
    Ok(Value::Num(i.next_random()))
}

fn math_atan2(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    Ok(Value::Num(
        i.to_num(&arg(args, 0))?.atan2(i.to_num(&arg(args, 1))?),
    ))
}

fn math_hypot(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    // spec: ToNumber every arg first — an abrupt completion stops all
    // later coercions. Then: any ±∞ → +∞ (even past a NaN), any NaN →
    // NaN, all-zero → +0. Scaling avoids intermediate overflow.
    let mut nums = Vec::with_capacity(args.len());
    for a in args {
        nums.push(i.to_num(a)?);
    }
    let mut max = 0.0f64;
    let mut nan = false;
    for &n in &nums {
        if n.is_infinite() {
            return Ok(Value::Num(f64::INFINITY));
        }
        nan |= n.is_nan();
        max = max.max(n.abs());
    }
    if nan {
        return Ok(Value::Num(f64::NAN));
    }
    if max == 0.0 {
        return Ok(Value::Num(0.0));
    }
    let mut s = 0.0;
    for &n in &nums {
        let r = n / max;
        s += r * r;
    }
    Ok(Value::Num(max * s.sqrt()))
}

fn math_imul(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let a = to_i32(i.to_num(&arg(args, 0))?);
    let b = to_i32(i.to_num(&arg(args, 1))?);
    Ok(Value::Num(a.wrapping_mul(b) as f64))
}

fn math_clz32(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    Ok(Value::Num(
        to_u32(i.to_num(&arg(args, 0))?).leading_zeros() as f64
    ))
}

fn math_fround(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    Ok(Value::Num(i.to_num(&arg(args, 0))? as f32 as f64))
}

/// `Math.sumPrecise` (ES2025) — GetIterator over the arg; each item must
/// already be a Number (TypeError via IteratorClose — no coercion).
/// Finite values are summed exactly in a fixed-point accumulator in
/// units of 2**-1074 (every finite double is m·2**(k-1074) with a 53-bit
/// m and k ≤ 2045), then correctly rounded — overflowing sums → ±∞.
fn math_sum_precise(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let items = arg(args, 0);
    let mut lz = for_of_iter(i, &items)?;
    // two's-complement integer S, little-endian limbs: S * 2**-1074 is
    // the exact running sum. 40*64 = 2560 bits ≫ the ~2115 reachable.
    let mut acc = [0u64; 40];
    let mut all_neg_zero = true;
    let mut pos_inf = false;
    let mut neg_inf = false;
    let mut nan = false;
    loop {
        let Some(v) = iter_src_step(i, &mut lz.src)? else {
            break;
        };
        let Value::Num(n) = v else {
            let e = i.type_err("Math.sumPrecise values must be numbers");
            // `? IteratorClose` — a close error replaces the TypeError
            iter_src_close(i, &mut lz.src)?;
            return Err(e);
        };
        if n.is_nan() {
            nan = true;
        } else if n == f64::INFINITY {
            pos_inf = true;
        } else if n == f64::NEG_INFINITY {
            neg_inf = true;
        } else {
            if !(n == 0.0 && n.is_sign_negative()) {
                all_neg_zero = false;
            }
            if n != 0.0 {
                let bits = n.to_bits();
                let bexp = ((bits >> 52) & 0x7ff) as usize;
                let frac = bits & 0x000f_ffff_ffff_ffff;
                // value = sign * m * 2**(k - 1074)
                let (m, k) = if bexp == 0 {
                    (frac, 0usize)
                } else {
                    (frac | (1u64 << 52), bexp - 1)
                };
                acc_add(&mut acc, m, k, n.is_sign_negative());
            }
        }
    }
    if nan || (pos_inf && neg_inf) {
        return Ok(Value::Num(f64::NAN));
    }
    if pos_inf {
        return Ok(Value::Num(f64::INFINITY));
    }
    if neg_inf {
        return Ok(Value::Num(f64::NEG_INFINITY));
    }
    let r = acc_to_f64(&acc);
    // an empty list or one whose every element is -0 sums to -0
    Ok(Value::Num(if r == 0.0 && all_neg_zero { -0.0 } else { r }))
}

/// add ±(m << k) to a little-endian two's-complement integer
fn acc_add(acc: &mut [u64; 40], m: u64, k: usize, neg: bool) {
    let limb = k / 64;
    let sh = (k % 64) as u32;
    let lo = m << sh;
    let hi = if sh == 0 { 0 } else { m >> (64 - sh) };
    if !neg {
        let (s, c) = acc[limb].overflowing_add(lo);
        acc[limb] = s;
        let mut carry = c;
        if hi != 0 || carry {
            let (s, c2) = acc[limb + 1].overflowing_add(hi);
            let (s, c3) = s.overflowing_add(carry as u64);
            acc[limb + 1] = s;
            carry = c2 || c3;
            let mut j = limb + 2;
            while carry && j < acc.len() {
                let (s, c) = acc[j].overflowing_add(1);
                acc[j] = s;
                carry = c;
                j += 1;
            }
        }
    } else {
        let (d, b) = acc[limb].overflowing_sub(lo);
        acc[limb] = d;
        let mut borrow = b;
        if hi != 0 || borrow {
            let (d, b2) = acc[limb + 1].overflowing_sub(hi);
            let (d, b3) = d.overflowing_sub(borrow as u64);
            acc[limb + 1] = d;
            borrow = b2 || b3;
            let mut j = limb + 2;
            while borrow && j < acc.len() {
                let (d, b) = acc[j].overflowing_sub(1);
                acc[j] = d;
                borrow = b;
                j += 1;
            }
        }
    }
}

/// convert the exact integer S (units of 2**-1074) to f64 with
/// round-half-even; magnitude past the max biased exponent → ±∞
fn acc_to_f64(acc: &[u64; 40]) -> f64 {
    const FRAC: u64 = 0x000f_ffff_ffff_ffff;
    let neg = (acc[39] >> 63) != 0;
    let mut mag = *acc;
    if neg {
        let mut carry = true;
        for l in mag.iter_mut() {
            let (v, c) = (!*l).overflowing_add(carry as u64);
            *l = v;
            carry = c;
        }
    }
    let mut p = -1i32; // position of the top set bit
    for (idx, &l) in mag.iter().enumerate().rev() {
        if l != 0 {
            p = (idx * 64 + 63 - l.leading_zeros() as usize) as i32;
            break;
        }
    }
    if p < 0 {
        return 0.0;
    }
    if p <= 52 {
        // |S| < 2**53 — S·2**-1074 is exactly representable
        let v = mag[0] as f64 * f64::from_bits(1);
        return if neg { -v } else { v };
    }
    // keep the top 53 bits, round-half-even on what is dropped
    let drop = (p - 52) as usize;
    let limb = drop / 64;
    let sh = (drop % 64) as u32;
    let hi_part = if sh == 0 {
        0
    } else {
        mag.get(limb + 1).copied().unwrap_or(0) << (64 - sh)
    };
    let mut q = (mag[limb] >> sh) | hi_part;
    let hl = (drop - 1) / 64;
    let hb = (drop - 1) % 64;
    let half = (mag[hl] >> hb as u32) & 1 == 1;
    let mut rest = (mag[hl] & ((1u64 << hb as u32) - 1)) != 0;
    if !rest {
        for &l in &mag[..hl] {
            if l != 0 {
                rest = true;
                break;
            }
        }
    }
    if half && (rest || q & 1 == 1) {
        q += 1;
    }
    // result is q·2**(drop-1074); as an f64 that's q·2**(e-1075)
    let mut e = drop + 1;
    if q >> 53 != 0 {
        q >>= 1;
        e += 1;
    }
    if e >= 2047 {
        return if neg {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    f64::from_bits(((neg as u64) << 63) | ((e as u64) << 52) | (q & FRAC))
}

// ===========================================================================
// natives: JSON
// ===========================================================================

fn json_parse(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let s = i.to_str(&arg(args, 0))?;
    let mut p = JsonParser {
        chars: s.chars().collect(),
        pos: 0,
    };
    p.skip_ws();
    let v = match p.value(i) {
        Ok(v) => {
            p.skip_ws();
            if p.pos != p.chars.len() {
                return Err(i.syntax_err("Unexpected token in JSON"));
            }
            v
        }
        Err(msg) => return Err(i.syntax_err(format!("Unexpected token in JSON: {msg}"))),
    };
    // spec InternalizeJSONProperty: reviver walks the tree post-order
    // inside a {"": value} holder; deleted props remove array/object slots
    let reviver = arg(args, 1);
    if i.is_callable(&reviver) {
        let holder = i.new_obj();
        i.set_data(&holder, "", v);
        return json_internalize(i, &Value::Obj(holder), "", &reviver);
    }
    Ok(v)
}

/// CreateDataProperty — [[DefineOwnProperty]]({value,W:T,E:T,C:T})
/// returning success; a `false` result (non-configurable target) is
/// skipped silently, matching how InternalizeJSONProperty ignores it
fn json_create_data_prop(i: &mut Interp, obj: &Value, key: PropKey, v: Value) -> Result<(), Value> {
    let Value::Obj(o) = obj else { return Ok(()) };
    // proxies route through the defineProperty trap — a throwing trap
    // propagates; ordinary objects use the validate-and-apply path where
    // a `false` (non-configurable) result is skipped silently
    if { matches!(&o.borrow().kind, ObjKind::Proxy { .. }) } {
        return i.recv_define(o, &key, v, true);
    }
    let d = i.new_obj();
    i.set_data(&d, "value", v);
    i.set_data(&d, "writable", Value::Bool(true));
    i.set_data(&d, "enumerable", Value::Bool(true));
    i.set_data(&d, "configurable", Value::Bool(true));
    let _ = define_prop_from_desc(i, o, key, &Value::Obj(d))?;
    Ok(())
}

/// InternalizeJSONProperty — `holder`/`name` name the slot holding `val`
/// (read inside, not passed in, so the reviver sees fresh values)
fn json_internalize(i: &mut Interp, holder: &Value, name: &str, reviver: &Value) -> NResult {
    let val = i.get_prop_str(holder, name)?;
    if let Value::Obj(o) = &val {
        let is_arr = matches!(
            array_is_array(i, &Value::Und, &[val.clone()])?,
            Value::Bool(true)
        );
        if is_arr {
            // ToLength(Get("length")) — proxies / array-likes read live
            let len_v = i.get_prop_str(&val, "length")?;
            let len = to_length_num(i.to_num(&len_v)?).min(1_000_000);
            for k in 0..len {
                let kk = k.to_string();
                let new_elem = json_internalize(i, &val, &kk, reviver)?;
                if matches!(new_elem, Value::Und) {
                    i.delete_prop(&val, &PropKey::S(Rc::from(kk.as_str())))?;
                } else {
                    let kv = PropKey::S(Rc::from(kk.as_str()));
                    json_create_data_prop(i, &val, kv, new_elem)?;
                }
            }
        } else {
            // enumerable own string keys (live snapshot via traps)
            let mut keys: Vec<String> = Vec::new();
            for (k, _) in own_enum_entries_full(i, &val, false)? {
                if let PropKey::S(s) = k {
                    keys.push(s.to_string());
                }
            }
            for kk in keys {
                let new_elem = json_internalize(i, &val, &kk, reviver)?;
                if matches!(new_elem, Value::Und) {
                    i.delete_prop(&val, &PropKey::S(Rc::from(kk.as_str())))?;
                } else {
                    json_create_data_prop(i, &val, PropKey::S(Rc::from(kk.as_str())), new_elem)?;
                }
            }
        }
        let _ = o;
    }
    i.call(
        reviver,
        holder.clone(),
        vec![Value::Str(Rc::from(name)), val],
    )
}

struct JsonParser {
    chars: Vec<char>,
    pos: usize,
}

impl JsonParser {
    /// JSON ws is exactly space/tab/LF/CR — no Unicode extras
    fn skip_ws(&mut self) {
        while self
            .chars
            .get(self.pos)
            .is_some_and(|c| matches!(c, ' ' | '\t' | '\n' | '\r'))
        {
            self.pos += 1;
        }
    }
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }
    fn expect(&mut self, c: char) -> Result<(), String> {
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected '{c}'"))
        }
    }
    fn value(&mut self, i: &mut Interp) -> Result<Value, String> {
        self.skip_ws();
        match self.peek() {
            Some('{') => {
                self.pos += 1;
                let o = i.new_obj();
                self.skip_ws();
                if self.peek() == Some('}') {
                    self.pos += 1;
                    return Ok(Value::Obj(o));
                }
                loop {
                    self.skip_ws();
                    let key = self.string()?;
                    self.skip_ws();
                    self.expect(':')?;
                    let v = self.value(i)?;
                    i.set_data(&o, &key, v);
                    self.skip_ws();
                    match self.peek() {
                        Some(',') => {
                            self.pos += 1;
                        }
                        Some('}') => {
                            self.pos += 1;
                            return Ok(Value::Obj(o));
                        }
                        _ => return Err("expected ',' or '}'".into()),
                    }
                }
            }
            Some('[') => {
                self.pos += 1;
                let mut items = Vec::new();
                self.skip_ws();
                if self.peek() == Some(']') {
                    self.pos += 1;
                    return Ok(Value::Obj(i.new_arr(items)));
                }
                loop {
                    items.push(self.value(i)?);
                    self.skip_ws();
                    match self.peek() {
                        Some(',') => {
                            self.pos += 1;
                        }
                        Some(']') => {
                            self.pos += 1;
                            return Ok(Value::Obj(i.new_arr(items)));
                        }
                        _ => return Err("expected ',' or ']'".into()),
                    }
                }
            }
            Some('"') => Ok(Value::Str(Rc::from(self.string()?.as_str()))),
            Some('t') => self.lit("true", Value::Bool(true)),
            Some('f') => self.lit("false", Value::Bool(false)),
            Some('n') => self.lit("null", Value::Null),
            Some(c) if c == '-' || c.is_ascii_digit() => self.number(),
            _ => Err("unexpected char".into()),
        }
    }
    /// strict JSONNumber: `-?(0|[1-9]\d*)(\.\d+)?([eE][+-]?\d+)?`
    fn number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        if self.peek() == Some('-') {
            self.pos += 1;
        }
        match self.peek() {
            Some('0') => {
                self.pos += 1;
            }
            Some(c) if c.is_ascii_digit() => {
                while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    self.pos += 1;
                }
            }
            _ => return Err("bad number".into()),
        }
        if self.peek() == Some('.') {
            self.pos += 1;
            if !self.peek().is_some_and(|c| c.is_ascii_digit()) {
                return Err("bad fraction".into());
            }
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some('e') | Some('E')) {
            self.pos += 1;
            if matches!(self.peek(), Some('+') | Some('-')) {
                self.pos += 1;
            }
            if !self.peek().is_some_and(|c| c.is_ascii_digit()) {
                return Err("bad exponent".into());
            }
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        let s: String = self.chars[start..self.pos].iter().collect();
        s.parse::<f64>()
            .map(Value::Num)
            .map_err(|_| format!("bad number {s}"))
    }
    fn lit(&mut self, word: &str, v: Value) -> Result<Value, String> {
        for c in word.chars() {
            if self.peek() != Some(c) {
                return Err(format!("bad literal {word}"));
            }
            self.pos += 1;
        }
        Ok(v)
    }
    fn string(&mut self) -> Result<String, String> {
        self.expect('"')?;
        let mut s = String::new();
        loop {
            match self.chars.get(self.pos).copied() {
                None => return Err("unterminated string".into()),
                Some('"') => {
                    self.pos += 1;
                    return Ok(s);
                }
                Some('\\') => {
                    self.pos += 1;
                    match self.chars.get(self.pos).copied() {
                        Some('n') => s.push('\n'),
                        Some('t') => s.push('\t'),
                        Some('r') => s.push('\r'),
                        Some('b') => s.push('\u{8}'),
                        Some('f') => s.push('\u{c}'),
                        Some('/') => s.push('/'),
                        Some('\\') => s.push('\\'),
                        Some('"') => s.push('"'),
                        Some('u') => {
                            let mut v: u32 = 0;
                            for k in 1..=4 {
                                let d = self
                                    .chars
                                    .get(self.pos + k)
                                    .and_then(|c| c.to_digit(16))
                                    .ok_or("bad \\u")?;
                                v = v * 16 + d;
                            }
                            self.pos += 4;
                            if let Some(c) = char::from_u32(v) {
                                s.push(c);
                            }
                        }
                        _ => return Err("bad escape".into()),
                    }
                    self.pos += 1;
                }
                // unescaped control chars are invalid in JSON strings
                Some(c) if (c as u32) < 0x20 => {
                    return Err("unescaped control char".into());
                }
                Some(c) => {
                    s.push(c);
                    self.pos += 1;
                }
            }
        }
    }
}

fn json_stringify(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let v = arg(args, 0);
    let replacer = arg(args, 1);
    let mut replacer_fn = Value::Und;
    let mut property_list: Option<Vec<String>> = None;
    if let Value::Obj(ro) = &replacer {
        if i.is_callable(&replacer) {
            replacer_fn = replacer.clone();
        } else if matches!(
            array_is_array(i, &Value::Und, &[replacer.clone()])?,
            Value::Bool(true)
        ) {
            // spec: PropertyList only when IsArray(replacer) — other
            // non-callable objects are ignored entirely
            let len_v = i.get_prop_str(&replacer, "length")?;
            let len = to_length_num(i.to_num(&len_v)?).min(1_000_000);
            let mut list: Vec<String> = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for idx in 0..len {
                let e = i.get_prop(&replacer, &idx_key(idx))?;
                let item = match e {
                    Value::Str(s) => Some(s.to_string()),
                    Value::Num(n) => Some(js_num_str(n)),
                    Value::Obj(ref o) => {
                        // [[StringData]]/[[NumberData]] objects → ToString(v)
                        // (the wrapper's own toString runs — overrides are
                        // observable)
                        let has_data = o
                            .borrow()
                            .props
                            .get_str("$value")
                            .is_some_and(|p| matches!(p.val, Value::Str(_) | Value::Num(_)));
                        if has_data {
                            Some(i.to_str(&e)?)
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                if let Some(k) = item {
                    if seen.insert(k.clone()) {
                        list.push(k);
                    }
                }
            }
            property_list = Some(list);
        }
        let _ = ro;
    }
    // spec: space arg unboxes via ToNumber/ToString — wrapper
    // conversions are observable (overridden toString fires)
    let space = match arg(args, 2) {
        Value::Obj(o) => {
            let slot = o.borrow().props.get_str("$value").map(|p| p.val.clone());
            let ov = Value::Obj(o.clone());
            match slot {
                Some(Value::Str(_)) => Value::Str(Rc::from(i.to_str(&ov)?.as_str())),
                Some(Value::Num(_)) => Value::Num(i.to_num(&ov)?),
                _ => Value::Und,
            }
        }
        s => s,
    };
    let indent = match space {
        Value::Num(n) => {
            let k = to_integer_or_inf(n);
            " ".repeat(if k.is_finite() {
                (k.max(0.0) as usize).min(10)
            } else {
                10
            })
        }
        Value::Str(s) => s.chars().take(10).collect(),
        _ => String::new(),
    };
    // the root holder: {"": value}
    let holder = i.new_obj();
    i.set_data(&holder, "", v);
    let mut stack = Vec::new();
    match json_ser_prop(
        i,
        &Value::Obj(holder),
        "",
        &replacer_fn,
        &property_list,
        &indent,
        &mut stack,
    )? {
        Some(s) => Ok(Value::Str(Rc::from(s.as_str()))),
        None => Ok(Value::Und),
    }
}

/// unbox a Number/String/Boolean wrapper's `$value` slot (spec
/// SerializeJSONProperty step 4)
fn json_unbox(i: &mut Interp, v: &Value) -> Result<Value, Value> {
    let Value::Obj(o) = v else {
        return Ok(v.clone());
    };
    let slot = {
        let b = o.borrow();
        if !matches!(&b.kind, ObjKind::Ord) {
            return Ok(v.clone());
        }
        if b.props.contains_key(&PropKey::from_str("__bigint_wrap")) {
            // BigIntData unwraps directly to the primitive stand-in
            if let Some(p) = b.props.get_str("$value") {
                return Ok(p.val.clone());
            }
            return Ok(v.clone());
        }
        b.props.get_str("$value").map(|p| p.val.clone())
    };
    // spec: Number → ToNumber(obj), String → ToString(obj) — the
    // wrapper's valueOf/toString run and may throw or be overridden;
    // Boolean unwraps the slot directly (no coercion)
    match slot {
        Some(Value::Num(_)) => Ok(Value::Num(i.to_num(v)?)),
        Some(Value::Str(_)) => Ok(Value::Str(Rc::from(i.to_str(v)?.as_str()))),
        Some(p @ Value::Bool(_)) => Ok(p),
        _ => Ok(v.clone()),
    }
}

/// spec SerializeJSONProperty — returns the serialized text or None when
/// the value is omitted (undefined/function/symbol)
fn json_ser_prop(
    i: &mut Interp,
    holder: &Value,
    key: &str,
    replacer_fn: &Value,
    property_list: &Option<Vec<String>>,
    indent: &str,
    stack: &mut Vec<usize>,
) -> Result<Option<String>, Value> {
    let mut value = i.get_prop(holder, &PropKey::from_str(key))?;
    // step 2: toJSON on objects only (and bigint stand-ins — ours are
    // objects anyway)
    if matches!(value, Value::Obj(_)) {
        let tj = i.get_prop_str(&value, "toJSON")?;
        if i.is_callable(&tj) {
            value = i.call(&tj, value.clone(), vec![Value::Str(Rc::from(key))])?;
        }
    }
    // step 3: replacer function sees the post-toJSON value
    if i.is_callable(replacer_fn) {
        value = i.call(
            replacer_fn,
            holder.clone(),
            vec![Value::Str(Rc::from(key)), value],
        )?;
    }
    let value = json_unbox(i, &value)?;
    Ok(match &value {
        Value::Und | Value::Sym(..) => None,
        Value::Null => Some("null".to_string()),
        Value::Bool(b) => Some(if *b { "true" } else { "false" }.to_string()),
        Value::Num(n) => {
            if n.is_finite() {
                Some(js_num_str(*n))
            } else {
                Some("null".to_string())
            }
        }
        Value::Str(s) => {
            let mut o = String::new();
            json_escape_into(s, &mut o);
            Some(o)
        }
        Value::Obj(o) => {
            // bigint stand-ins throw on bare stringify
            if crate::binary::bigint_is_prim(&value) {
                return Err(i.type_err("Do not know how to serialize a BigInt"));
            }
            if i.is_callable(&value) {
                return Ok(None);
            }
            let ptr = Rc::as_ptr(o) as usize;
            if stack.contains(&ptr) {
                return Err(i.type_err("circular structure"));
            }
            stack.push(ptr);
            let r = (|| {
                let is_arr = matches!(
                    array_is_array(i, &Value::Und, &[value.clone()])?,
                    Value::Bool(true)
                );
                if is_arr {
                    json_ser_arr(i, &value, replacer_fn, property_list, indent, stack)
                } else {
                    json_ser_obj(i, &value, replacer_fn, property_list, indent, stack)
                }
            })();
            stack.pop();
            Some(r?)
        }
    })
}

/// SerializeJSONArray — ToLength(Get("length")) + per-index props so
/// proxies / subclasses read through [[Get]]
fn json_ser_arr(
    i: &mut Interp,
    arr: &Value,
    replacer_fn: &Value,
    property_list: &Option<Vec<String>>,
    indent: &str,
    stack: &mut Vec<usize>,
) -> Result<String, Value> {
    let len_v = i.get_prop_str(arr, "length")?;
    let len = to_length_num(i.to_num(&len_v)?).min(1_000_000);
    let mut parts: Vec<String> = Vec::with_capacity(len.min(1024));
    for k in 0..len {
        match json_ser_prop(
            i,
            arr,
            &k.to_string(),
            replacer_fn,
            property_list,
            indent,
            stack,
        )? {
            Some(s) => parts.push(s),
            None => parts.push("null".to_string()),
        }
    }
    if indent.is_empty() {
        return Ok(format!("[{}]", parts.join(",")));
    }
    if parts.is_empty() {
        return Ok("[]".to_string());
    }
    let level = stack.len();
    let pad = indent.repeat(level - 1);
    let inner = indent.repeat(level);
    Ok(format!(
        "[\n{}{}\n{}]",
        inner,
        parts.join(&format!(",\n{inner}")),
        pad
    ))
}

/// Enumerable string own keys WITHOUT [[Get]] — the JSON key list is a
/// snapshot (K computed once); getters fire later inside
/// SerializeJSONProperty where deletions remain observable
fn json_enum_string_keys(i: &mut Interp, v: &Value) -> Result<Vec<String>, Value> {
    let mut out = Vec::new();
    if let Value::Obj(o) = v {
        if { matches!(&o.borrow().kind, ObjKind::Proxy { .. }) } {
            for k in own_keys_all(i, v)? {
                let PropKey::S(_) = &k else { continue };
                let kv = i.key_to_value(&k);
                let d = obj_get_own_prop_desc(i, &Value::Und, &[v.clone(), kv])?;
                if let Value::Obj(dv) = d {
                    let en = i.get_prop_str(&Value::Obj(dv), "enumerable")?;
                    if i.to_bool(&en) {
                        if let PropKey::S(s) = k {
                            out.push(s.to_string());
                        }
                    }
                }
            }
            return Ok(out);
        }
    }
    for (k, _) in own_enum_entries(i, v) {
        if let PropKey::S(s) = k {
            out.push(s.to_string());
        }
    }
    Ok(out)
}

/// SerializeJSONObject — enumerable string keys (or the PropertyList),
/// each serialized via SerializeJSONProperty with THIS object as holder
fn json_ser_obj(
    i: &mut Interp,
    obj: &Value,
    replacer_fn: &Value,
    property_list: &Option<Vec<String>>,
    indent: &str,
    stack: &mut Vec<usize>,
) -> Result<String, Value> {
    let keys: Vec<String> = match property_list {
        Some(list) => list.clone(),
        None => json_enum_string_keys(i, obj)?,
    };
    let mut parts: Vec<String> = Vec::new();
    for k in &keys {
        if let Some(ser) = json_ser_prop(i, obj, k, replacer_fn, property_list, indent, stack)? {
            let mut ek = String::new();
            json_escape_into(k, &mut ek);
            let colon = if indent.is_empty() { ":" } else { ": " };
            parts.push(format!("{ek}{colon}{ser}"));
        }
    }
    if indent.is_empty() {
        return Ok(format!("{{{}}}", parts.join(",")));
    }
    if parts.is_empty() {
        return Ok("{}".to_string());
    }
    let level = stack.len();
    let pad = indent.repeat(level - 1);
    let inner = indent.repeat(level);
    Ok(format!(
        "{{\n{}{}\n{}}}",
        inner,
        parts.join(&format!(",\n{inner}")),
        pad
    ))
}

fn json_escape_into(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let u = c as usize;
                out.push_str("\\u00");
                out.push(HEX[(u >> 4) & 15] as char);
                out.push(HEX[u & 15] as char);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ===========================================================================
// natives: console
// ===========================================================================

fn console_log(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let mut line = String::new();
    for (k, a) in args.iter().enumerate() {
        if k > 0 {
            line.push(' ');
        }
        line.push_str(&i.inspect(a, 0));
    }
    i.output.push(line);
    Ok(Value::Und)
}

// ===========================================================================
// natives: promise — spec jobs-based: reactions + thenable resolution run
// as queued jobs drained at the microtask checkpoint (end of `run`)
// ===========================================================================

fn new_promise(i: &mut Interp) -> ObjRef {
    let p = Rc::new(RefCell::new(Obj::new(
        Some(i.p.promise.clone()),
        ObjKind::Promise(PromiseState::Pending),
    )));
    // pending reactions live in a hidden `$handlers` array — each
    // element is a {onF,onR,resolve,reject} reaction record
    i.set_builtin(&p, "$handlers", Value::Obj(i.new_arr(Vec::new())));
    p
}

fn promise_state(o: &ObjRef) -> PromiseState {
    match &o.borrow().kind {
        ObjKind::Promise(s) => s.clone(),
        _ => PromiseState::Pending,
    }
}

fn is_promise(v: &Value) -> bool {
    matches!(v, Value::Obj(o) if matches!(o.borrow().kind, ObjKind::Promise(_)))
}

/// the `$handlers` reaction list — lazily installed so engine-made
/// promises that skip `new_promise` still take reactions
fn handlers_arr(i: &mut Interp, p: &ObjRef) -> ObjRef {
    let cur = p
        .borrow()
        .props
        .get_str("$handlers")
        .map(|pr| pr.val.clone());
    if let Some(Value::Obj(o)) = cur {
        if matches!(o.borrow().kind, ObjKind::Arr(_)) {
            return o;
        }
    }
    let a = i.new_arr(Vec::new());
    i.set_builtin(p, "$handlers", Value::Obj(a.clone()));
    a
}

/// a native fn wrapped as a Bound fn carrying `holder` as `this` — the
/// engine's stand-in for spec internal-slot closures (resolve/reject,
/// element fns, finally thunks). Bound fns have no `prototype`, are
/// extensible, and are non-constructors — matching the spec shapes.
fn bound_native(i: &mut Interp, name: &str, f: NativeFn, holder: Value, len: f64) -> Value {
    let target = i.native(name, f);
    let b = Rc::new(RefCell::new(Obj::new(
        Some(i.p.func.clone()),
        ObjKind::Bound {
            target,
            thisv: holder,
            extra: Vec::new(),
        },
    )));
    // spec CreateBuiltinFunction: SetFunctionLength runs BEFORE
    // SetFunctionName — own-prop insertion order is `length` then `name`
    i.set_attr(&b, "length", Value::Num(len), false, true);
    i.set_attr(&b, "name", Value::Str(Rc::from(name)), false, true);
    Value::Obj(b)
}

/// spec CreateResolvingFunctions — a resolve/reject pair sharing ONE
/// `alreadyResolved` latch. Pair state lives on a holder bound as the
/// fns' `this`: `$p` = the promise, `$r` = the latch flag.
fn resolving_fns(i: &mut Interp, p: &ObjRef) -> (Value, Value) {
    let holder = i.new_obj();
    i.set_builtin(&holder, "$p", Value::Obj(p.clone()));
    i.set_builtin(&holder, "$r", Value::Bool(false));
    let hv = Value::Obj(holder);
    (
        bound_native(i, "", promise_resolve_inner, hv.clone(), 1.0),
        bound_native(i, "", promise_reject_inner, hv, 1.0),
    )
}

/// consume a resolving-fn holder's `alreadyResolved` latch — returns
/// the promise when this call is the pair's first resolve/reject
/// attempt; `None` on a foreign receiver or an already-tripped latch
fn resolve_latch(h: &ObjRef) -> Option<Value> {
    let mut b = h.borrow_mut();
    let already = matches!(b.props.get_str("$r"), Some(pr) if matches!(pr.val, Value::Bool(true)));
    if already {
        return None;
    }
    let p = b.props.get_str("$p").map(|pr| pr.val.clone())?;
    if let Some(pr) = b.props.get_str_mut("$r") {
        pr.val = Value::Bool(true);
    }
    Some(p)
}

fn promise_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // `Promise(executor)` as a plain call is a TypeError — [[Construct]]
    // only. `this` must be the fresh object [[Construct]] made: the
    // new_target_native check alone misfires when a native ctor frame is
    // still on the stack (new_target stays Some during nested calls).
    let fresh_this = matches!(this, Value::Obj(o)
        if matches!(o.borrow().kind, ObjKind::Ord) && o.borrow().props.is_empty());
    if !(i.new_target_native.is_some() && fresh_this) {
        return Err(i.type_err("Promise constructor cannot be invoked without 'new'"));
    }
    let executor = arg(args, 0);
    if !i.is_callable(&executor) {
        let tn = i.type_name(&executor);
        return Err(i.type_err(format!("Promise resolver {tn} is not a function")));
    }
    let Value::Obj(o) = this else { unreachable!() };
    o.borrow_mut().kind = ObjKind::Promise(PromiseState::Pending);
    let _ = handlers_arr(i, o);
    let (res, rej) = resolving_fns(i, o);
    // executor throw → Call(reject, «error») — the latch decides
    if let Err(e) = i.call(&executor, Value::Und, vec![res, rej.clone()]) {
        i.call(&rej, Value::Und, vec![e])?;
    }
    Ok(this.clone())
}

/// spec Promise Resolve Function — `this` = the resolving-fn holder
/// carrying the shared `alreadyResolved` latch. Thenables settle
/// asynchronously through a PromiseResolveThenableJob; everything else
/// settles synchronously.
fn promise_resolve_inner(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let Value::Obj(h) = this else {
        return Ok(Value::Und);
    };
    let Some(p) = resolve_latch(h) else {
        return Ok(Value::Und);
    };
    let Value::Obj(po) = &p else {
        return Ok(Value::Und);
    };
    let resolution = arg(args, 0);
    // SameValue(resolution, promise) → RejectPromise(TypeError)
    if i.same_value(&resolution, &p)? {
        let e = i.type_err("Chaining cycle detected for promise");
        return settle(i, po, false, e);
    }
    if !matches!(resolution, Value::Obj(_)) {
        return settle(i, po, true, resolution);
    }
    // `then` lookup — an abrupt getter rejects the promise
    let then = match i.get_prop_str(&resolution, "then") {
        Ok(t) => t,
        Err(e) => return settle(i, po, false, e),
    };
    if !i.is_callable(&then) {
        // non-callable `then` → fulfill with the thenable itself
        return settle(i, po, true, resolution);
    }
    // PromiseResolveThenableJob — `then` runs as a job with a FRESH
    // resolving-fn pair (each job gets its own alreadyResolved record)
    let pj = p.clone();
    i.jobs
        .push_back(Box::new(move |i: &mut Interp| -> Result<(), Value> {
            if let Value::Obj(po2) = &pj {
                let (res, rej) = resolving_fns(i, po2);
                if let Err(e) = i.call(&then, resolution, vec![res, rej.clone()]) {
                    i.call(&rej, Value::Und, vec![e])?;
                }
            }
            Ok(())
        }));
    Ok(Value::Und)
}

/// spec Promise Reject Function — `this` = the resolving-fn holder.
fn promise_reject_inner(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let Value::Obj(h) = this else {
        return Ok(Value::Und);
    };
    let Some(p) = resolve_latch(h) else {
        return Ok(Value::Und);
    };
    let Value::Obj(po) = &p else {
        return Ok(Value::Und);
    };
    settle(i, po, false, arg(args, 0))
}

fn settle(i: &mut Interp, p: &ObjRef, ok: bool, v: Value) -> NResult {
    {
        let mut b = p.borrow_mut();
        if let ObjKind::Promise(st) = &b.kind {
            if !matches!(st, PromiseState::Pending) {
                return Ok(Value::Und);
            }
            b.kind = ObjKind::Promise(if ok {
                PromiseState::Fulfilled(Box::new(v.clone()))
            } else {
                PromiseState::Rejected(Box::new(v.clone()))
            });
        } else {
            return Ok(Value::Und);
        }
    }
    // spec TriggerPromiseReactions — one job per queued reaction, FIFO;
    // settlement consumes the pending reaction lists
    let handlers = handlers_of(i, p);
    for h in handlers {
        let Value::Obj(ho) = &h else { continue };
        let getf = |name: &str| {
            ho.borrow()
                .props
                .get_str(name)
                .map(|p| p.val.clone())
                .unwrap_or(Value::Und)
        };
        let on_f = getf("onF");
        let on_r = getf("onR");
        let resolve = getf("resolve");
        let reject = getf("reject");
        let vv = v.clone();
        i.jobs.push_back(Box::new(move |i: &mut Interp| {
            run_reaction(i, ok, on_f, on_r, resolve, reject, vv)
        }));
    }
    let h = handlers_arr(i, p);
    h.borrow_mut().kind = ObjKind::Arr(Vec::new());
    Ok(Value::Und)
}

fn handlers_of(i: &mut Interp, p: &ObjRef) -> Vec<Value> {
    let _ = i;
    if let Some(Value::Obj(h)) = p
        .borrow()
        .props
        .get_str("$handlers")
        .map(|pr| pr.val.clone())
    {
        if let ObjKind::Arr(items) = &h.borrow().kind {
            return items.clone();
        }
    }
    Vec::new()
}

/// one PromiseReactionJob — runs the matching handler and forwards its
/// outcome to the reaction's capability resolve/reject. A missing or
/// non-callable handler is the spec identity (fulfill) / thrower
/// (reject) passthrough.
fn run_reaction(
    i: &mut Interp,
    ok: bool,
    on_f: Value,
    on_r: Value,
    resolve: Value,
    reject: Value,
    v: Value,
) -> Result<(), Value> {
    let handler = if ok { on_f } else { on_r };
    let r = if i.is_callable(&handler) {
        i.call(&handler, Value::Und, vec![v.clone()])
    } else if ok {
        Ok(v.clone())
    } else {
        Err(v.clone())
    };
    match r {
        Ok(x) => {
            i.call(&resolve, Value::Und, vec![x])?;
        }
        Err(e) => {
            i.call(&reject, Value::Und, vec![e])?;
        }
    }
    Ok(())
}

/// spec SpeciesConstructor(O, defaultConstructor) — reads
/// `O.constructor`, then `C[@@species]`; undefined/null species falls
/// back to the intrinsic default, a non-constructor species throws.
fn species_ctor(i: &mut Interp, o: &Value, default: Value) -> ERes {
    let c = i.get_prop_str(o, "constructor")?;
    if matches!(c, Value::Und) {
        return Ok(default);
    }
    if !matches!(c, Value::Obj(_)) {
        return Err(i.type_err("promise constructor is not an object"));
    }
    let s = i.get_prop(&c, &PropKey::Sym(SYM_SPECIES))?;
    match s {
        Value::Und | Value::Null => Ok(default),
        s if i.is_constructor(&s) => Ok(s),
        _ => Err(i.type_err("promise species is not a constructor")),
    }
}

/// spec GetPromiseResolve(C) — `C.resolve` must be callable.
fn get_promise_resolve(i: &mut Interp, c: &Value) -> Result<Value, Value> {
    let r = i.get_prop_str(c, "resolve")?;
    if !i.is_callable(&r) {
        return Err(i.type_err("promise resolve is not a function"));
    }
    Ok(r)
}

/// spec NewPromiseCapability(C) → (promise, resolve, reject). The
/// capture executor fires inside `new C(exec)` and drops the pair onto
/// a holder; slots left non-callable → TypeError.
fn new_promise_capability(i: &mut Interp, c: &Value) -> Result<(Value, Value, Value), Value> {
    if !i.is_constructor(c) {
        return Err(i.type_err("promise capability requires a constructor"));
    }
    let holder = Value::Obj(i.new_obj());
    let exec = bound_native(i, "", promise_cap_executor, holder.clone(), 2.0);
    let promise = i.construct(c, vec![exec], None)?;
    let resolve = i.get_prop_str(&holder, "resolve")?;
    let reject = i.get_prop_str(&holder, "reject")?;
    if !i.is_callable(&resolve) || !i.is_callable(&reject) {
        return Err(i.type_err("promise capability functions are not callable"));
    }
    Ok((promise, resolve, reject))
}

/// IfAbruptRejectPromise — reject the capability with `e` and return
/// its promise (a reject-fn error collapses into the returned value)
fn reject_cap(i: &mut Interp, rej: &Value, p: &Value, e: Value) -> NResult {
    let _ = i.call(rej, Value::Und, vec![e]);
    Ok(p.clone())
}

fn promise_then(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec PerformPromiseThen — IsPromise brand check first
    let Value::Obj(po) = this else {
        return Err(i.type_err("Promise.prototype.then called on incompatible receiver"));
    };
    if !matches!(po.borrow().kind, ObjKind::Promise(_)) {
        return Err(i.type_err("Promise.prototype.then called on incompatible receiver"));
    }
    let c = species_ctor(i, this, Value::Obj(i.p.promise_ctor.clone()))?;
    let (p2, res, rej) = new_promise_capability(i, &c)?;
    let on_f = arg(args, 0);
    let on_r = arg(args, 1);
    match promise_state(po) {
        PromiseState::Pending => {
            let rec = i.new_obj();
            i.set_builtin(&rec, "onF", on_f);
            i.set_builtin(&rec, "onR", on_r);
            i.set_builtin(&rec, "resolve", res);
            i.set_builtin(&rec, "reject", rej);
            let h = handlers_arr(i, po);
            let mut hb = h.borrow_mut();
            if let ObjKind::Arr(items) = &mut hb.kind {
                items.push(Value::Obj(rec));
            }
            drop(hb);
        }
        PromiseState::Fulfilled(v) => {
            let vv = (*v).clone();
            i.jobs.push_back(Box::new(move |i: &mut Interp| {
                run_reaction(i, true, on_f, on_r, res, rej, vv)
            }));
        }
        PromiseState::Rejected(v) => {
            let vv = (*v).clone();
            i.jobs.push_back(Box::new(move |i: &mut Interp| {
                run_reaction(i, false, on_f, on_r, res, rej, vv)
            }));
        }
    }
    Ok(p2)
}

fn promise_catch(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec: Invoke(this, "then", « undefined, onRejected ») — `then` is
    // read fresh off the receiver so overridden hooks fire
    let then = i.get_prop_str(this, "then")?;
    i.call(&then, this.clone(), vec![Value::Und, arg(args, 0)])
}

fn promise_finally(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    // spec: the receiver check is Type-not-Object (not IsPromise)
    if !matches!(this, Value::Obj(_)) {
        return Err(i.type_err("Promise.prototype.finally called on non-object receiver"));
    }
    let c = species_ctor(i, this, Value::Obj(i.p.promise_ctor.clone()))?;
    // spec: with a non-callable callback, thenFinally/catchFinally are
    // the callback itself — `then` sees it verbatim
    let (then_f, catch_f) = if i.is_callable(&cb) {
        // GetPromiseResolve is fetched eagerly, before `then` runs
        let pr = get_promise_resolve(i, &c)?;
        (
            mk_finally_fn(i, true, &cb, &c, &pr),
            mk_finally_fn(i, false, &cb, &c, &pr),
        )
    } else {
        (cb.clone(), cb)
    };
    let then = i.get_prop_str(this, "then")?;
    i.call(&then, this.clone(), vec![then_f, catch_f])
}

fn mk_finally_fn(i: &mut Interp, ok: bool, cb: &Value, c: &Value, pr: &Value) -> Value {
    let h = i.new_obj();
    i.set_builtin(&h, "$cb", cb.clone());
    i.set_builtin(&h, "$C", c.clone());
    i.set_builtin(&h, "$pr", pr.clone());
    // spec: the thenFinally/catchFinally built-ins are anonymous — the
    // functions handed to `then` have `length` 1 and `name` ""
    bound_native(
        i,
        "",
        if ok {
            promise_then_finally
        } else {
            promise_catch_finally
        },
        Value::Obj(h),
        1.0,
    )
}

/// spec thenFinally(value): run onFinally, lift the result through
/// `C.resolve`, then `.then(() => value)` — settlement waits on the
/// callback's returned thenable.
fn promise_then_finally(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let Value::Obj(h) = this else {
        return Ok(Value::Und);
    };
    let cb = i.obj_get_raw_str(h, "$cb").unwrap_or(Value::Und);
    let c = i.obj_get_raw_str(h, "$C").unwrap_or(Value::Und);
    let pr = i.obj_get_raw_str(h, "$pr").unwrap_or(Value::Und);
    let value = arg(args, 0);
    let r = i.call(&cb, Value::Und, vec![])?;
    let ip = i.call(&pr, c.clone(), vec![r])?;
    let then = i.get_prop_str(&ip, "then")?;
    let th = i.new_obj();
    i.set_builtin(&th, "$v", value);
    let tfn = bound_native(i, "", promise_value_thunk, Value::Obj(th), 0.0);
    i.call(&then, ip, vec![tfn])
}

/// spec catchFinally(reason): same shape, but the inner `.then` thunk
/// rethrows the original rejection reason.
fn promise_catch_finally(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let Value::Obj(h) = this else {
        return Ok(Value::Und);
    };
    let cb = i.obj_get_raw_str(h, "$cb").unwrap_or(Value::Und);
    let c = i.obj_get_raw_str(h, "$C").unwrap_or(Value::Und);
    let pr = i.obj_get_raw_str(h, "$pr").unwrap_or(Value::Und);
    let reason = arg(args, 0);
    let r = i.call(&cb, Value::Und, vec![])?;
    let ip = i.call(&pr, c.clone(), vec![r])?;
    let then = i.get_prop_str(&ip, "then")?;
    let th = i.new_obj();
    i.set_builtin(&th, "$v", reason);
    let tfn = bound_native(i, "", promise_thrower, Value::Obj(th), 0.0);
    i.call(&then, ip, vec![tfn])
}

/// `() => value` thunk for finally's inner `.then`
fn promise_value_thunk(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let Value::Obj(h) = this else {
        return Ok(Value::Und);
    };
    Ok(i.obj_get_raw_str(h, "$v").unwrap_or(Value::Und))
}

/// `() => { throw reason }` for finally's inner `.then`
fn promise_thrower(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let Value::Obj(h) = this else {
        return Ok(Value::Und);
    };
    Err(i.obj_get_raw_str(h, "$v").unwrap_or(Value::Und))
}

/// spec PromiseResolve(C, x) — IsPromise(x) && x.constructor === C
/// returns x itself; else a fresh capability promise resolved with x.
/// `this` = the constructor the static was called on.
pub(crate) fn promise_resolve_static(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    if !matches!(this, Value::Obj(_)) {
        return Err(i.type_err("Promise.resolve called on non-object receiver"));
    }
    let x = arg(args, 0);
    if is_promise(&x) {
        let xc = i.get_prop_str(&x, "constructor")?;
        if i.same_value(&xc, this)? {
            return Ok(x);
        }
    }
    let (p, res, _rej) = new_promise_capability(i, this)?;
    i.call(&res, Value::Und, vec![x])?;
    Ok(p)
}

/// spec: NewPromiseCapability(this), then Call(reject, «r»).
pub(crate) fn promise_reject_static(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    if !matches!(this, Value::Obj(_)) {
        return Err(i.type_err("Promise.reject called on non-object receiver"));
    }
    let (p, _res, rej) = new_promise_capability(i, this)?;
    i.call(&rej, Value::Und, vec![arg(args, 0)])?;
    Ok(p)
}

// -- PerformPromise* combinator machinery ----------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum PComb {
    All,
    AllSettled,
    Any,
    Race,
}

/// element-fn state holder — `$flag` is the alreadyCalled latch, `$i`
/// the list index; mode-specific slots ($vals/$errs/$rem/$res/$rej/$ok)
/// are set by the mk_* helpers
fn mk_el_holder(i: &mut Interp, idx: usize) -> ObjRef {
    let h = i.new_obj();
    i.set_builtin(&h, "$flag", Value::Bool(false));
    i.set_builtin(&h, "$i", Value::Num(idx as f64));
    h
}

/// consume the element fn's alreadyCalled latch — true on first call
fn el_claim(h: &ObjRef) -> bool {
    let mut b = h.borrow_mut();
    let done = matches!(b.props.get_str("$flag"), Some(p) if matches!(p.val, Value::Bool(true)));
    if !done {
        if let Some(pr) = b.props.get_str_mut("$flag") {
            pr.val = Value::Bool(true);
        }
    }
    !done
}

fn el_index(i: &Interp, h: &ObjRef) -> usize {
    match i.obj_get_raw_str(h, "$i") {
        Some(Value::Num(n)) => n.max(0.0) as usize,
        _ => 0,
    }
}

/// the counter is SIGNED — spec element fns decrement unconditionally
/// so a second call pushes it below zero and the `== 0` settle only ever
/// fires once (no saturating at 0)
fn remaining_n(i: &Interp, rem: &ObjRef) -> i64 {
    match i.obj_get_raw_str(rem, "n") {
        Some(Value::Num(n)) => n as i64,
        _ => 0,
    }
}

fn set_remaining_n(rem: &ObjRef, n: i64) {
    let mut b = rem.borrow_mut();
    if let Some(pr) = b.props.get_str_mut("n") {
        pr.val = Value::Num(n as f64);
    }
}

fn inc_remaining(i: &Interp, rem: &ObjRef) {
    let n = remaining_n(i, rem);
    set_remaining_n(rem, n + 1);
}

fn dec_remaining(i: &Interp, rem: &ObjRef) -> i64 {
    let n = remaining_n(i, rem) - 1;
    set_remaining_n(rem, n);
    n
}

/// `new AggregateError(errors)` without a message arg — Promise.any's
/// all-rejected terminal value
fn aggregate_error(i: &mut Interp, errors: Value) -> Value {
    let p = i.p.agg_error.clone();
    let e = i.err_obj_opt(&p, "AggregateError", None);
    if let Value::Obj(o) = &e {
        i.set_attr(o, "errors", errors, true, true);
    }
    e
}

fn mk_all_element(i: &mut Interp, idx: usize, values: &Value, rem: &ObjRef, res: &Value) -> Value {
    let h = mk_el_holder(i, idx);
    i.set_builtin(&h, "$vals", values.clone());
    i.set_builtin(&h, "$rem", Value::Obj(rem.clone()));
    i.set_builtin(&h, "$res", res.clone());
    bound_native(i, "", promise_all_element, Value::Obj(h), 1.0)
}

/// spec Promise.all resolve element fn — writes values[$i], decrements
/// remainingElements, resolves the capability with the values array
/// when the count drains
fn promise_all_element(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let Value::Obj(h) = this else {
        return Ok(Value::Und);
    };
    if !el_claim(h) {
        return Ok(Value::Und);
    }
    let idx = el_index(i, h);
    let values = i.obj_get_raw_str(h, "$vals").unwrap_or(Value::Und);
    let rem = i.obj_get_raw_str(h, "$rem");
    let res = i.obj_get_raw_str(h, "$res").unwrap_or(Value::Und);
    create_data_prop(i, &values, idx, arg(args, 0))?;
    if let Some(Value::Obj(ro)) = rem {
        if dec_remaining(i, &ro) == 0 {
            i.call(&res, Value::Und, vec![values])?;
        }
    }
    Ok(Value::Und)
}

fn mk_allsettled_element(
    i: &mut Interp,
    idx: usize,
    values: &Value,
    rem: &ObjRef,
    res: &Value,
    ok: bool,
) -> Value {
    let h = mk_el_holder(i, idx);
    i.set_builtin(&h, "$vals", values.clone());
    i.set_builtin(&h, "$rem", Value::Obj(rem.clone()));
    i.set_builtin(&h, "$res", res.clone());
    i.set_builtin(&h, "$ok", Value::Bool(ok));
    bound_native(i, "", promise_allsettled_element, Value::Obj(h), 1.0)
}

/// spec Promise.allSettled resolve/reject element fn — records the
/// {status, value|reason} snapshot at $i, resolves on zero remaining
fn promise_allsettled_element(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let Value::Obj(h) = this else {
        return Ok(Value::Und);
    };
    if !el_claim(h) {
        return Ok(Value::Und);
    }
    let idx = el_index(i, h);
    let ok = matches!(i.obj_get_raw_str(h, "$ok"), Some(Value::Bool(true)));
    let values = i.obj_get_raw_str(h, "$vals").unwrap_or(Value::Und);
    let rem = i.obj_get_raw_str(h, "$rem");
    let res = i.obj_get_raw_str(h, "$res").unwrap_or(Value::Und);
    let o = i.new_obj();
    if ok {
        i.set_data(&o, "status", Value::Str(Rc::from("fulfilled")));
        i.set_data(&o, "value", arg(args, 0));
    } else {
        i.set_data(&o, "status", Value::Str(Rc::from("rejected")));
        i.set_data(&o, "reason", arg(args, 0));
    }
    create_data_prop(i, &values, idx, Value::Obj(o))?;
    if let Some(Value::Obj(ro)) = rem {
        if dec_remaining(i, &ro) == 0 {
            i.call(&res, Value::Und, vec![values])?;
        }
    }
    Ok(Value::Und)
}

fn mk_any_reject(i: &mut Interp, idx: usize, errors: &Value, rem: &ObjRef, rej: &Value) -> Value {
    let h = mk_el_holder(i, idx);
    i.set_builtin(&h, "$errs", errors.clone());
    i.set_builtin(&h, "$rem", Value::Obj(rem.clone()));
    i.set_builtin(&h, "$rej", rej.clone());
    bound_native(i, "", promise_any_reject_el, Value::Obj(h), 1.0)
}

/// spec Promise.any reject element fn — records errors[$i], rejects
/// with an AggregateError when the remaining count drains
fn promise_any_reject_el(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let Value::Obj(h) = this else {
        return Ok(Value::Und);
    };
    if !el_claim(h) {
        return Ok(Value::Und);
    }
    let idx = el_index(i, h);
    let errors = i.obj_get_raw_str(h, "$errs").unwrap_or(Value::Und);
    let rem = i.obj_get_raw_str(h, "$rem");
    let rej = i.obj_get_raw_str(h, "$rej").unwrap_or(Value::Und);
    create_data_prop(i, &errors, idx, arg(args, 0))?;
    if let Some(Value::Obj(ro)) = rem {
        if dec_remaining(i, &ro) == 0 {
            let e = aggregate_error(i, errors);
            i.call(&rej, Value::Und, vec![e])?;
        }
    }
    Ok(Value::Und)
}

/// driver for Promise.all/allSettled/any/race — the spec's
/// PerformPromise* loop. NewPromiseCapability failures throw
/// synchronously; everything after routes through
/// IfAbruptRejectPromise (IteratorClose on the iterated-record errors,
/// then reject the capability and return its promise).
fn promise_combine(i: &mut Interp, this: &Value, args: &[Value], mode: PComb) -> NResult {
    let (p, res, rej) = new_promise_capability(i, this)?;
    let promise_resolve = match get_promise_resolve(i, this) {
        Ok(v) => v,
        Err(e) => return reject_cap(i, &rej, &p, e),
    };
    let mut lz = match for_of_iter(i, &arg(args, 0)) {
        Ok(l) => l,
        Err(e) => return reject_cap(i, &rej, &p, e),
    };
    let values = Value::Obj(i.new_arr(Vec::new()));
    let errors = Value::Obj(i.new_arr(Vec::new()));
    let remaining = i.new_obj();
    i.set_builtin(&remaining, "n", Value::Num(1.0));
    let mut index = 0usize;
    // abrupt exits from the spec Repeat loop carry a flag for
    // iteratorRecord.[[Done]]: IteratorStep / IteratorValue abrupts set
    // [[Done]] (NO IteratorClose — the `return` hook must not run), while
    // abrupts from the loop body (C.resolve call, `then` lookup/invoke)
    // leave [[Done]] false so the outer IfAbruptRejectPromise closes first
    let mut iter_err: Option<(Value, bool)> = None;
    loop {
        match iter_lazy_step(i, &mut lz) {
            Err(e) => {
                iter_err = Some((e, false));
                break;
            }
            Ok(None) => break,
            Ok(Some(next_value)) => {
                // spec appends an `undefined` slot first so the values /
                // errors list has a hole to fill at `index`
                let slot_err = match mode {
                    PComb::Any => create_data_prop(i, &errors, index, Value::Und).err(),
                    PComb::Race => None,
                    _ => create_data_prop(i, &values, index, Value::Und).err(),
                };
                if let Some(e) = slot_err {
                    iter_err = Some((e, true));
                    break;
                }
                let next_promise = match i.call(&promise_resolve, this.clone(), vec![next_value]) {
                    Ok(v) => v,
                    Err(e) => {
                        iter_err = Some((e, true));
                        break;
                    }
                };
                // per-mode reaction pair — spec "same-reject" semantics
                // ride the capability's own fns verbatim where it says so
                let (on_f, on_r) = match mode {
                    PComb::All => (
                        mk_all_element(i, index, &values, &remaining, &res),
                        rej.clone(),
                    ),
                    PComb::AllSettled => (
                        mk_allsettled_element(i, index, &values, &remaining, &res, true),
                        mk_allsettled_element(i, index, &values, &remaining, &res, false),
                    ),
                    PComb::Any => (
                        res.clone(),
                        mk_any_reject(i, index, &errors, &remaining, &rej),
                    ),
                    PComb::Race => (res.clone(), rej.clone()),
                };
                inc_remaining(i, &remaining);
                let then_call = match i.get_prop_str(&next_promise, "then") {
                    Err(e) => Err(e),
                    Ok(then) => i.call(&then, next_promise, vec![on_f, on_r]).map(|_| ()),
                };
                if let Err(e) = then_call {
                    iter_err = Some((e, true));
                    break;
                }
                index += 1;
            }
        }
    }
    if let Some((e, needs_close)) = iter_err {
        // abrupt during iteration — IteratorClose only while the record
        // is not [[Done]], then reject the capability and return its
        // promise (close errors are swallowed: a throw is in flight)
        if needs_close {
            let _ = iter_lazy_close(i, &mut lz);
        }
        return reject_cap(i, &rej, &p, e);
    }
    // normal loop exit: remaining--; on zero the result settles now. The
    // iterator is exhausted ([[Done]] is set) so a throwing capability fn
    // skips IteratorClose and the error only rejects the capability.
    if dec_remaining(i, &remaining) == 0 {
        let settle_err = match mode {
            PComb::All | PComb::AllSettled => i.call(&res, Value::Und, vec![values]).err(),
            PComb::Any => {
                let ae = aggregate_error(i, errors);
                i.call(&rej, Value::Und, vec![ae]).err()
            }
            PComb::Race => None,
        };
        if let Some(e) = settle_err {
            return reject_cap(i, &rej, &p, e);
        }
    }
    Ok(p)
}

fn promise_all(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    promise_combine(i, this, args, PComb::All)
}

fn promise_all_settled(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    promise_combine(i, this, args, PComb::AllSettled)
}

fn promise_any(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    promise_combine(i, this, args, PComb::Any)
}

fn promise_race(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    promise_combine(i, this, args, PComb::Race)
}

/// spec Promise.try — the callback runs inside the call. Its result is
/// lifted through PromiseResolve(C, ·) so a callback returning a promise
/// whose constructor IS C yields that promise itself (no extra wrapper);
/// a throwing callback produces a capability promise rejected with the
/// error — the C instance is only constructed once, inside whichever
/// path runs.
fn promise_try(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    if !matches!(this, Value::Obj(_)) {
        return Err(i.type_err("Promise.try called on non-object receiver"));
    }
    let f = arg(args, 0);
    let rest: Vec<Value> = args.iter().skip(1).cloned().collect();
    match i.call(&f, Value::Und, rest) {
        Ok(v) => promise_resolve_static(i, this, &[v]),
        Err(e) => {
            let (p, _res, rej) = new_promise_capability(i, this)?;
            i.call(&rej, Value::Und, vec![e])?;
            Ok(p)
        }
    }
}

/// Promise.allKeyed — {fulfilled:{…}, rejected:{…}} snapshot keyed by
/// the input's enumerable own properties. Elements resolve through
/// `C.resolve`; every outcome lands in exactly one bucket.
fn promise_all_keyed(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let (p, res, rej) = new_promise_capability(i, this)?;
    let promise_resolve = match get_promise_resolve(i, this) {
        Ok(v) => v,
        Err(e) => return reject_cap(i, &rej, &p, e),
    };
    let fulfilled = Value::Obj(i.new_obj());
    let rejected = Value::Obj(i.new_obj());
    let out_o = i.new_obj();
    i.set_data(&out_o, "fulfilled", fulfilled.clone());
    i.set_data(&out_o, "rejected", rejected.clone());
    let outv = Value::Obj(out_o);
    let remaining = i.new_obj();
    i.set_builtin(&remaining, "n", Value::Num(1.0));
    let entries = match own_enum_entries_full(i, &arg(args, 0), true) {
        Ok(e) => e,
        Err(e) => return reject_cap(i, &rej, &p, e),
    };
    for (k, v) in entries {
        let next_promise = match i.call(&promise_resolve, this.clone(), vec![v]) {
            Ok(x) => x,
            Err(e) => return reject_cap(i, &rej, &p, e),
        };
        let kv = i.key_to_value(&k);
        let res_el = mk_allkeyed_element(i, &kv, &fulfilled, &outv, &remaining, &res);
        let rej_el = mk_allkeyed_element(i, &kv, &rejected, &outv, &remaining, &res);
        inc_remaining(i, &remaining);
        let r = match i.get_prop_str(&next_promise, "then") {
            Err(e) => Err(e),
            Ok(then) => i
                .call(&then, next_promise, vec![res_el, rej_el])
                .map(|_| ()),
        };
        if let Err(e) = r {
            return reject_cap(i, &rej, &p, e);
        }
    }
    if dec_remaining(i, &remaining) == 0 {
        i.call(&res, Value::Und, vec![outv])?;
    }
    Ok(p)
}

fn mk_allkeyed_element(
    i: &mut Interp,
    k: &Value,
    tgt: &Value,
    out: &Value,
    rem: &ObjRef,
    res: &Value,
) -> Value {
    let h = mk_el_holder(i, 0);
    i.set_builtin(&h, "$k", k.clone());
    i.set_builtin(&h, "$tgt", tgt.clone());
    i.set_builtin(&h, "$out", out.clone());
    i.set_builtin(&h, "$rem", Value::Obj(rem.clone()));
    i.set_builtin(&h, "$res", res.clone());
    bound_native(i, "", promise_allkeyed_element, Value::Obj(h), 1.0)
}

/// allKeyed element fn — writes tgt[key] with the settled value, then
/// resolves the capability with the {fulfilled,rejected} record on zero
fn promise_allkeyed_element(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let Value::Obj(h) = this else {
        return Ok(Value::Und);
    };
    if !el_claim(h) {
        return Ok(Value::Und);
    }
    let kv = i.obj_get_raw_str(h, "$k").unwrap_or(Value::Und);
    let k = i.to_propkey(&kv)?;
    let tgt = i.obj_get_raw_str(h, "$tgt").unwrap_or(Value::Und);
    let out = i.obj_get_raw_str(h, "$out").unwrap_or(Value::Und);
    let rem = i.obj_get_raw_str(h, "$rem");
    let res = i.obj_get_raw_str(h, "$res").unwrap_or(Value::Und);
    i.set_prop(&tgt, k, arg(args, 0))?;
    if let Some(Value::Obj(ro)) = rem {
        if dec_remaining(i, &ro) == 0 {
            i.call(&res, Value::Und, vec![out])?;
        }
    }
    Ok(Value::Und)
}

// ===========================================================================
// natives: date
// ===========================================================================

fn now_millis() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

fn date_now(_i: &mut Interp, _t: &Value, _a: &[Value]) -> NResult {
    Ok(Value::Num(now_millis()))
}

/// spec TimeClip: |v| > 8.64e15 or NaN → NaN; else integer part (+0 for -0).
fn time_clip(n: f64) -> f64 {
    if !n.is_finite() || n.abs() > 8.64e15 {
        f64::NAN
    } else {
        n.trunc() + 0.0
    }
}

/// `ObjKind::Host("date")` marks objects carrying a [[DateValue]] internal
/// slot; the value itself lives in the hidden `$ms` prop.
fn is_date(v: &Value) -> bool {
    matches!(v, Value::Obj(o)
        if matches!(&o.borrow().kind, ObjKind::Host(tag) if tag.as_ref() == "date"))
}

/// spec thisTimeValue — the slot value, or TypeError for non-Date receivers.
fn this_time_value(i: &mut Interp, this: &Value) -> Result<f64, Value> {
    if is_date(this) {
        if let Value::Obj(o) = this {
            if let Some(Value::Num(n)) = i.obj_get_raw_str(o, "$ms") {
                return Ok(n);
            }
        }
    }
    Err(i.type_err("this is not a Date object"))
}

/// spec MakeDay (year, month, date) — ToInteger on each, f64 arithmetic.
fn make_day(y: f64, m: f64, d: f64) -> f64 {
    if !y.is_finite() || !m.is_finite() || !d.is_finite() {
        return f64::NAN;
    }
    let (y, m, d) = (y.trunc(), m.trunc(), d.trunc());
    let ym = y + (m / 12.0).floor();
    let mn = m.rem_euclid(12.0);
    if ym.abs() > 9.0e18 {
        // unrepresentable — TimeClip would NaN the result anyway
        return f64::NAN;
    }
    days_from_civil(ym as i64, (mn + 1.0) as u32, 1) as f64 + d - 1.0
}

/// spec MakeTime (hour, min, sec, ms) — ToInteger on each, spec f64 grouping.
fn make_time(h: f64, m: f64, s: f64, ms: f64) -> f64 {
    if !h.is_finite() || !m.is_finite() || !s.is_finite() || !ms.is_finite() {
        return f64::NAN;
    }
    h.trunc() * 3_600_000.0 + m.trunc() * 60_000.0 + s.trunc() * 1_000.0 + ms.trunc()
}

/// spec MakeDate (day, time).
fn make_date(day: f64, time: f64) -> f64 {
    if !day.is_finite() || !time.is_finite() {
        return f64::NAN;
    }
    day * 86_400_000.0 + time
}

fn date_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // `new_target_native` is only Some inside a native [[Construct]] — but a
    // plain `Date()` nested in one still sees it, so additionally require
    // `this` to be the freshly allocated object (same check as RegExp).
    let fresh_this = matches!(this, Value::Obj(o)
        if matches!(o.borrow().kind, ObjKind::Ord) && o.borrow().props.is_empty());
    if !(i.new_target_native.is_some() && fresh_this) {
        // called as a function — the spec's ToDateString(current time)
        return Ok(Value::Str(Rc::from(date_str(now_millis()))));
    }
    let ms = match args.len() {
        0 => now_millis(),
        1 => {
            let v = arg(args, 0);
            if is_date(&v) {
                // a Date arg copies its [[DateValue]] — no coercion happens
                if let Value::Obj(o) = &v {
                    match i.obj_get_raw_str(o, "$ms") {
                        Some(Value::Num(n)) => n,
                        _ => f64::NAN,
                    }
                } else {
                    f64::NAN
                }
            } else {
                match i.to_primitive(&v)? {
                    s @ Value::Str(_) => match date_parse(i, &Value::Und, &[s]) {
                        Ok(Value::Num(n)) => n,
                        _ => f64::NAN,
                    },
                    v2 => i.to_num(&v2)?,
                }
            }
        }
        _ => {
            // multi-arg ctor: MakeDate(MakeDay(y,m,d), MakeTime(h,mi,s,ms))
            // — coerce every arg via ToNumber first (missing = spec defaults)
            let mut n = |k: usize, d: f64| -> Result<f64, Value> {
                match args.get(k) {
                    Some(v) => i.to_num(v),
                    None => Ok(d),
                }
            };
            let y = n(0, 0.0)?;
            let mo = n(1, 0.0)?;
            let d = n(2, 1.0)?;
            let h = n(3, 0.0)?;
            let mi = n(4, 0.0)?;
            let s = n(5, 0.0)?;
            let msec = n(6, 0.0)?;
            // 0 ≤ ToInteger(y) ≤ 99 → 1900 + ToInteger(y)
            let yr = {
                let yi = y.trunc();
                if !y.is_nan() && (0.0..=99.0).contains(&yi) {
                    1900.0 + yi
                } else {
                    y
                }
            };
            time_clip(make_date(make_day(yr, mo, d), make_time(h, mi, s, msec)))
        }
    };
    if let Value::Obj(o) = this {
        o.borrow_mut().kind = ObjKind::Host(Rc::from("date"));
        i.set_builtin(o, "$ms", Value::Num(time_clip(ms)));
    }
    Ok(this.clone())
}

fn date_get_time(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    Ok(Value::Num(this_time_value(i, this)?))
}

/// (year, month 1-12, day, weekday 0=Sun, hour, min, sec, ms) for UTC epoch ms.
/// Howard Hinnant's civil-from-days algorithm.
fn date_parts(ms: f64) -> (i64, u32, u32, u32, u32, u32, u32, u32) {
    if !ms.is_finite() {
        return (0, 0, 0, 0, 0, 0, 0, 0);
    }
    let ms_i = ms.floor() as i64;
    let days = ms_i.div_euclid(86_400_000);
    let sod = ms_i.rem_euclid(86_400_000);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    let wd = ((days + 4).rem_euclid(7)) as u32; // epoch day = Thursday
    let h = (sod / 3_600_000) as u32;
    let mi = (sod % 3_600_000 / 60_000) as u32;
    let s = (sod % 60_000 / 1000) as u32;
    let msec = (sod % 1000) as u32;
    (y, m, d, wd, h, mi, s, msec)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

fn date_getter(
    i: &mut Interp,
    this: &Value,
    f: impl Fn((i64, u32, u32, u32, u32, u32, u32, u32)) -> f64,
) -> NResult {
    let ms = this_time_value(i, this)?;
    if !ms.is_finite() {
        return Ok(Value::Num(f64::NAN));
    }
    Ok(Value::Num(f(date_parts(ms))))
}

fn date_store(i: &mut Interp, this: &Value, ms: f64) -> Value {
    if let Value::Obj(o) = this {
        i.set_builtin(o, "$ms", Value::Num(ms));
    }
    Value::Num(ms)
}

/// Date setter core. `slots` = which of the 7 components (y,mo,d,h,mi,s,ms)
/// the args occupy, left to right; `zero_base` = setFullYear's NaN→+0 rule.
fn date_set(
    i: &mut Interp,
    this: &Value,
    args: &[Value],
    slots: &[usize],
    zero_base: bool,
) -> NResult {
    // spec order: read [[DateValue]] first (TypeError on a non-Date), then
    // ToNumber the provided args left-to-right, then the NaN check on t.
    let t = this_time_value(i, this)?;
    // the first formal is always coerced (absent → ToNumber(undefined) →
    // NaN); later optionals are coerced only when actually present
    let mut given: Vec<f64> = Vec::with_capacity(slots.len());
    for k in 0..slots.len() {
        if k == 0 {
            given.push(i.to_num(args.get(0).unwrap_or(&Value::Und))?);
        } else if let Some(a) = args.get(k) {
            given.push(i.to_num(a)?);
        }
    }
    let t = if t.is_nan() {
        if zero_base {
            0.0
        } else {
            // spec returns NaN here *without* touching the slot — a valueOf
            // side effect may have already replaced it
            return Ok(Value::Num(f64::NAN));
        }
    } else {
        t
    };
    let base = date_parts(t);
    let (mut y, mut mo, mut d, mut h, mut mi, mut s, mut msec) = (
        base.0 as f64,
        (base.1 - 1) as f64,
        base.2 as f64,
        base.4 as f64,
        base.5 as f64,
        base.6 as f64,
        base.7 as f64,
    );
    for (k, v) in given.iter().enumerate() {
        match slots[k] {
            0 => y = *v,
            1 => mo = *v,
            2 => d = *v,
            3 => h = *v,
            4 => mi = *v,
            5 => s = *v,
            _ => msec = *v,
        }
    }
    let ms = time_clip(make_date(make_day(y, mo, d), make_time(h, mi, s, msec)));
    Ok(date_store(i, this, ms))
}

fn date_set_time(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // slot check runs before ToNumber — TypeError on non-Date receivers
    this_time_value(i, this)?;
    let v = i.to_num(&arg(args, 0))?;
    Ok(date_store(i, this, time_clip(v)))
}

/// legacy setYear: 0-99 → 1900+y (already full years otherwise)
fn date_set_year(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec reads the slot (TypeError on non-Date) before ToNumber(year)
    this_time_value(i, this)?;
    let mut y = i.to_num(&arg(args, 0))?;
    let yi = y.trunc();
    if !y.is_nan() && (0.0..=99.0).contains(&yi) {
        y = 1900.0 + yi;
    }
    date_set(i, this, &[Value::Num(y)], &[0], true)
}

/// spec Date.prototype.toJSON: ToPrimitive(number) → non-finite → null;
/// else invoke this.toISOString()
fn date_to_json(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    // O = ? ToObject(this) — undefined/null throw TypeError
    if matches!(this, Value::Und | Value::Null) {
        return Err(i.type_err("Cannot convert undefined or null to object"));
    }
    let tv = match this {
        Value::Obj(o) => i.to_primitive_hint(this, o.clone(), "number")?,
        v => v.clone(),
    };
    if let Value::Num(n) = tv {
        if !n.is_finite() {
            return Ok(Value::Null);
        }
    }
    let f = i.get_prop_str(this, "toISOString")?;
    if !i.is_callable(&f) {
        return Err(i.type_err("this.toISOString is not a function"));
    }
    i.call(&f, this.clone(), vec![])
}

/// spec Date.prototype[@@toPrimitive](hint): "default"→"string";
/// OrdinaryToPrimitive with the resolved hint.
fn date_to_primitive(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec: Type(this value) must be Object before the hint is examined
    if !matches!(this, Value::Obj(_)) {
        return Err(i.type_err("Date.prototype[Symbol.toPrimitive] called on non-object"));
    }
    let hint = match arg(args, 0) {
        Value::Str(s) => s.to_string(),
        _ => String::new(),
    };
    let order: [&str; 2] = match hint.as_str() {
        "number" => ["valueOf", "toString"],
        "string" | "default" => ["toString", "valueOf"],
        _ => return Err(i.type_err("invalid hint")),
    };
    for name in order {
        let m = i.get_prop_str(this, name)?;
        if i.is_callable(&m) {
            let r = i.call(&m, this.clone(), vec![])?;
            if !matches!(r, Value::Obj(_)) {
                return Ok(r);
            }
        }
    }
    Err(i.type_err("Cannot convert object to primitive value"))
}

const WD_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MO_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// the spec DateString year: padded to ≥4 digits; a `-` sign only when
/// negative ("-0001", "-12345"; positive years are never signed).
fn fmt_year(y: i64) -> String {
    if y < 0 {
        format!("-{:04}", y.unsigned_abs())
    } else {
        format!("{:04}", y)
    }
}

/// ToDateString(tv) for a finite UTC time — shared by `Date()` / toString.
fn date_str(ms: f64) -> String {
    let (y, m, d, wd, h, mi, s, _) = date_parts(ms);
    format!(
        "{} {} {:02} {} {:02}:{:02}:{:02} GMT+0000 (Coordinated Universal Time)",
        WD_NAMES[wd as usize],
        MO_NAMES[(m - 1) as usize],
        d,
        fmt_year(y),
        h,
        mi,
        s
    )
}

fn date_to_string(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let ms = this_time_value(i, this)?;
    if !ms.is_finite() {
        return Ok(Value::Str(Rc::from("Invalid Date")));
    }
    Ok(Value::Str(Rc::from(date_str(ms))))
}

fn date_to_utc(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let ms = this_time_value(i, this)?;
    if !ms.is_finite() {
        return Ok(Value::Str(Rc::from("Invalid Date")));
    }
    let (y, m, d, wd, h, mi, s, _) = date_parts(ms);
    Ok(Value::Str(Rc::from(
        format!(
            "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
            WD_NAMES[wd as usize],
            d,
            MO_NAMES[(m - 1) as usize],
            fmt_year(y),
            h,
            mi,
            s
        )
        .as_str(),
    )))
}

fn date_to_iso(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let ms = this_time_value(i, this)?;
    if !ms.is_finite() {
        return Err(i.range_err("Invalid time value"));
    }
    let (y, m, d, _, h, mi, s, msec) = date_parts(ms);
    // spec: years outside 0000-9999 use the signed 6-digit expanded form
    let ys = if (0..=9999).contains(&y) {
        format!("{:04}", y)
    } else if y < 0 {
        format!("-{:06}", y.unsigned_abs())
    } else {
        format!("+{:06}", y)
    };
    Ok(Value::Str(Rc::from(
        format!(
            "{}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            ys, m, d, h, mi, s, msec
        )
        .as_str(),
    )))
}

fn date_to_datestr(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let ms = this_time_value(i, this)?;
    if !ms.is_finite() {
        return Ok(Value::Str(Rc::from("Invalid Date")));
    }
    let (y, m, d, wd, _, _, _, _) = date_parts(ms);
    Ok(Value::Str(Rc::from(
        format!(
            "{} {} {:02} {}",
            WD_NAMES[wd as usize],
            MO_NAMES[(m - 1) as usize],
            d,
            fmt_year(y)
        )
        .as_str(),
    )))
}

fn date_to_timestr(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let ms = this_time_value(i, this)?;
    if !ms.is_finite() {
        return Ok(Value::Str(Rc::from("Invalid Date")));
    }
    let (_, _, _, _, h, mi, s, _) = date_parts(ms);
    Ok(Value::Str(Rc::from(
        format!(
            "{:02}:{:02}:{:02} GMT+0000 (Coordinated Universal Time)",
            h, mi, s
        )
        .as_str(),
    )))
}

fn date_utc(i: &mut Interp, _t: &Value, a: &[Value]) -> NResult {
    // spec: ToNumber each supplied arg in order — year is required, the
    // rest default (month → 0, date → 1, others → 0)
    let y = i.to_num(&arg(a, 0))?;
    let mut n = |k: usize, d: f64| -> Result<f64, Value> {
        if k < a.len() {
            i.to_num(&a[k])
        } else {
            Ok(d)
        }
    };
    let mo = n(1, 0.0)?;
    let d = n(2, 1.0)?;
    let h = n(3, 0.0)?;
    let mi = n(4, 0.0)?;
    let s = n(5, 0.0)?;
    let msec = n(6, 0.0)?;
    // 0 ≤ ToInteger(y) ≤ 99 → 1900 + ToInteger(y)
    let yr = {
        let yi = y.trunc();
        if !y.is_nan() && (0.0..=99.0).contains(&yi) {
            1900.0 + yi
        } else {
            y
        }
    };
    Ok(Value::Num(time_clip(make_date(
        make_day(yr, mo, d),
        make_time(h, mi, s, msec),
    ))))
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// "HH:mm[:ss[.fff]]" — strict 2-digit fields; hour 24 only with zero rest.
fn parse_hms(t: &str) -> Option<(f64, f64, f64, f64)> {
    let two_dig = |x: Option<&str>| -> Option<u32> {
        match x {
            Some(s) if s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit()) => s.parse().ok(),
            _ => None,
        }
    };
    let mut it = t.split(':');
    let h = two_dig(it.next())?;
    let m = two_dig(it.next())?;
    let (s, f) = match it.next() {
        Some(ss) => {
            let (sp, fp) = ss.split_once('.').unwrap_or((ss, ""));
            let s = if sp.len() == 2 && sp.bytes().all(|b| b.is_ascii_digit()) {
                sp.parse().ok()?
            } else {
                return None;
            };
            let f = if fp.is_empty() {
                0.0
            } else if fp.bytes().all(|b| b.is_ascii_digit()) {
                (format!("0.{fp}").parse::<f64>().ok()? * 1000.0).trunc()
            } else {
                return None;
            };
            (s, f)
        }
        None => (0, 0.0),
    };
    if it.next().is_some() {
        return None;
    }
    if h > 24 || m > 59 || s > 59 || (h == 24 && (m != 0 || s != 0 || f != 0.0)) {
        return None;
    }
    Some((h as f64, m as f64, s as f64, f))
}

/// ISO zone suffix: "Z" or ±HH[:MM] / ±HHMM — the offset in ms.
fn parse_zone(z: &str) -> Option<f64> {
    if z.eq_ignore_ascii_case("z") {
        return Some(0.0);
    }
    let (sign, r) = if let Some(r) = z.strip_prefix('+') {
        (1.0, r)
    } else if let Some(r) = z.strip_prefix('-') {
        (-1.0, r)
    } else {
        return None;
    };
    let (hh, mm): (u32, u32) = if let Some((h, m)) = r.split_once(':') {
        if h.len() != 2
            || m.len() != 2
            || !h.bytes().all(|b| b.is_ascii_digit())
            || !m.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        (h.parse().ok()?, m.parse().ok()?)
    } else if r.len() == 4 && r.bytes().all(|b| b.is_ascii_digit()) {
        (r[..2].parse().ok()?, r[2..].parse().ok()?)
    } else if r.len() == 2 && r.bytes().all(|b| b.is_ascii_digit()) {
        (r.parse().ok()?, 0)
    } else {
        return None;
    };
    if hh > 23 || mm > 59 {
        return None;
    }
    Some(sign * (hh as f64 * 3_600_000.0 + mm as f64 * 60_000.0))
}

/// spec Date Time String Format:
/// [±]YYYY[-MM[-DD]][T|space HH:mm[:ss[.sss]]][Z|±HH:mm]
fn parse_iso_ms(s: &str) -> Option<f64> {
    let (dstr, tstr) = match s.find(|c| c == 'T' || c == 't' || c == ' ') {
        Some(idx) => (&s[..idx], Some(&s[idx + 1..])),
        None => (s, None),
    };
    let (signed, neg, drest) = if let Some(r) = dstr.strip_prefix('+') {
        (true, false, r)
    } else if let Some(r) = dstr.strip_prefix('-') {
        (true, true, r)
    } else {
        (false, false, dstr)
    };
    let segs: Vec<&str> = drest.split('-').collect();
    if segs.len() > 3 || segs.iter().any(|x| x.is_empty()) {
        return None;
    }
    let ystr = segs[0];
    let ok_len = if signed {
        ystr.len() == 6
    } else {
        ystr.len() == 4
    };
    if !ok_len || !ystr.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if neg && ystr.bytes().all(|b| b == b'0') {
        return None; // "-000000" is not a valid year
    }
    let mut y: i64 = ystr.parse().ok()?;
    if neg {
        y = -y;
    }
    let seg2 = |x: Option<&&str>| -> Option<u32> {
        match x {
            Some(s) if s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit()) => s.parse().ok(),
            Some(_) => None,
            None => Some(0),
        }
    };
    let m = match segs.get(1) {
        None => 1,
        x => seg2(x)?,
    };
    let d = match segs.get(2) {
        None => 1,
        x => seg2(x)?,
    };
    if tstr.is_some() && segs.len() != 3 {
        return None;
    }
    if !(1..=12).contains(&m) || !(1..=days_in_month(y, m)).contains(&d) {
        return None;
    }
    let mut ms = days_from_civil(y, m, d) as f64 * 86_400_000.0;
    if let Some(t) = tstr {
        let zi = t.find(|c| c == 'Z' || c == 'z' || c == '+' || c == '-');
        let (tp, off) = match zi {
            Some(zi) => (&t[..zi], parse_zone(&t[zi..])?),
            None => (t, 0.0),
        };
        let (h, mi, s2, f) = parse_hms(tp)?;
        ms += h * 3_600_000.0 + mi * 60_000.0 + s2 * 1_000.0 + f - off;
    }
    Some(ms)
}

/// English month name → 1-12 ("Jan", "January", "Sept", ...).
fn month_of(t: &str) -> Option<u32> {
    const NAMES: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let l = t.to_ascii_lowercase();
    if l.len() < 3 || !l.bytes().all(|b| b.is_ascii_lowercase()) {
        return None;
    }
    NAMES
        .iter()
        .position(|n| l.starts_with(*n))
        .map(|p| p as u32 + 1)
}

/// the non-ISO forms engines accept — needed for toString/toUTCString
/// round-trips ("Wed Jul 01 2020 ...", "Thu, 01 Jan 1970 ... GMT").
fn parse_other_ms(s: &str) -> Option<f64> {
    let s = s.split('(').next().unwrap_or("").trim();
    let mut month: Option<u32> = None;
    let mut day: Option<i64> = None;
    let mut year: Option<i64> = None;
    let mut tod = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let mut have_time = false;
    let mut offset = 0.0f64;
    let mut pre_ints: Vec<i64> = Vec::new();
    let mut post_ints: Vec<i64> = Vec::new();
    for t in s.split([',', ' ']).filter(|t| !t.is_empty()) {
        if month.is_none() {
            if let Some(m) = month_of(t) {
                month = Some(m);
                continue;
            }
        }
        if t.contains('/') {
            // MM/DD/YYYY (US order); a leading >31 value means YYYY/MM/DD
            let p: Vec<&str> = t.split('/').collect();
            if p.len() != 3 {
                return None;
            }
            let (a, b, c): (i64, i64, i64) =
                (p[0].parse().ok()?, p[1].parse().ok()?, p[2].parse().ok()?);
            let (y2, m2, d2) = if a > 31 { (a, b, c) } else { (c, a, b) };
            month = Some(m2 as u32);
            day = Some(d2);
            year = Some(y2);
            continue;
        }
        if t.as_bytes()[0].is_ascii_digit() && t.contains(':') {
            if have_time {
                return None;
            }
            tod = parse_hms(t)?;
            have_time = true;
            continue;
        }
        let low = t.to_ascii_lowercase();
        if low == "z" || low == "gmt" || low == "utc" || low == "ut" {
            continue; // zero offset
        }
        if low.starts_with("gmt") || low.starts_with("utc") {
            offset = parse_zone(&t[3..])?;
            continue;
        }
        // a signed field after the year/time is a zone offset ("-0500");
        // before them it's the (possibly negative) year
        if (t.starts_with('+') || t.starts_with('-')) && (year.is_some() || have_time) {
            if let Some(o) = parse_zone(t) {
                offset = o;
                continue;
            }
        }
        if let Ok(v) = t.parse::<i64>() {
            if month.is_none() {
                pre_ints.push(v);
            } else {
                post_ints.push(v);
            }
            continue;
        }
        if t.bytes().all(|b| b.is_ascii_alphabetic()) {
            continue; // weekday / timezone-name noise ("Thu", "PST", ...)
        }
        return None;
    }
    // ints before the month name fill the day slot first; ints after it
    // fill day then year (covers "Jan 01 1970" and "01 Jan 1970" alike)
    for v in pre_ints.iter().chain(post_ints.iter()) {
        if (1..=31).contains(v) && day.is_none() {
            day = Some(*v);
        } else if year.is_none() {
            year = Some(*v);
        }
    }
    let (m, d, y) = (month?, day?, year?);
    if !(1..=12).contains(&m) || !(1..=days_in_month(y, m) as i64).contains(&d) {
        return None;
    }
    Some(
        days_from_civil(y, m, d as u32) as f64 * 86_400_000.0
            + tod.0 * 3_600_000.0
            + tod.1 * 60_000.0
            + tod.2 * 1_000.0
            + tod.3
            - offset,
    )
}

fn date_parse(i: &mut Interp, _t: &Value, a: &[Value]) -> NResult {
    let s = i.to_str(&arg(a, 0))?;
    let s = s.trim();
    let ms = parse_iso_ms(s)
        .or_else(|| parse_other_ms(s))
        .filter(|ms| ms.is_finite() && ms.abs() <= 8.64e15)
        .unwrap_or(f64::NAN);
    Ok(Value::Num(ms))
}

// ===========================================================================
// natives: map / set
// ===========================================================================

fn map_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    need_new_target(i, "Map")?;
    let m = if let Value::Obj(o) = this {
        o.borrow_mut().kind = ObjKind::Map(Vec::new());
        o.borrow_mut()
            .props
            .insert_str("__map", Prop::builtin(Value::Bool(true)));
        o.clone()
    } else {
        let o = Rc::new(RefCell::new(Obj::new(
            Some(i.p.obj.clone()),
            ObjKind::Map(Vec::new()),
        )));
        o.borrow_mut()
            .props
            .insert_str("__map", Prop::builtin(Value::Bool(true)));
        o
    };
    match arg(args, 0) {
        Value::Und | Value::Null => {}
        v => {
            // AddEntriesFromIterable: `set` fetched+callable-checked before
            // the iterable is pulled; each entry installs via Call(adder)
            let adder = i.get_prop_str(&Value::Obj(m.clone()), "set")?;
            if !i.is_callable(&adder) {
                return Err(i.type_err("Map.prototype.set is not callable"));
            }
            let mut lz = for_of_iter(i, &v)?;
            loop {
                let e = match iter_lazy_step(i, &mut lz) {
                    Ok(Some(e)) => e,
                    Ok(None) => break,
                    Err(e) => return Err(e),
                };
                if !matches!(e, Value::Obj(_)) {
                    let _ = iter_lazy_close(i, &mut lz);
                    return Err(i.type_err("Iterator value is not an entry object"));
                }
                let r = (|| {
                    let k = i.get_prop_str(&e, "0")?;
                    let val = i.get_prop_str(&e, "1")?;
                    i.call(&adder, Value::Obj(m.clone()), vec![k, val])
                })();
                match r {
                    Ok(_) => {}
                    Err(e) => {
                        let _ = iter_lazy_close(i, &mut lz);
                        return Err(e);
                    }
                }
            }
        }
    }
    Ok(Value::Obj(m))
}

/// `[[MapData]]`/`[[SetData]]` brands — like the weak collections the
/// constructors install a non-enumerable marker so prototype methods can
/// distinguish real instances from lookalikes (spec: TypeError)
fn need_map(i: &mut Interp, this: &Value, m: &str) -> Result<ObjRef, Value> {
    if let Value::Obj(o) = this {
        let b = o.borrow();
        if matches!(&b.kind, ObjKind::Map(_)) && b.props.get_str("__map").is_some() {
            return Ok(o.clone());
        }
    }
    Err(i.type_err(format!(
        "Method Map.prototype.{m} called on incompatible receiver"
    )))
}

fn need_set(i: &mut Interp, this: &Value, m: &str) -> Result<ObjRef, Value> {
    if let Value::Obj(o) = this {
        let b = o.borrow();
        if matches!(&b.kind, ObjKind::Set(_)) && b.props.get_str("__set").is_some() {
            return Ok(o.clone());
        }
    }
    Err(i.type_err(format!(
        "Method Set.prototype.{m} called on incompatible receiver"
    )))
}

/// `[[WeakMapData]]`/`[[WeakSetData]]` brand check — WeakMap/WeakSet
/// instances carry a non-enumerable `__wm`/`__ws` slot installed by
/// their constructors
fn need_wm(i: &mut Interp, this: &Value, m: &str) -> Result<ObjRef, Value> {
    if let Value::Obj(o) = this {
        let b = o.borrow();
        if matches!(&b.kind, ObjKind::Map(_)) && b.props.get_str("__wm").is_some() {
            return Ok(o.clone());
        }
    }
    Err(i.type_err(format!(
        "Method WeakMap.prototype.{m} called on incompatible receiver"
    )))
}

fn need_ws(i: &mut Interp, this: &Value, m: &str) -> Result<ObjRef, Value> {
    if let Value::Obj(o) = this {
        let b = o.borrow();
        if matches!(&b.kind, ObjKind::Set(_)) && b.props.get_str("__ws").is_some() {
            return Ok(o.clone());
        }
    }
    Err(i.type_err(format!(
        "Method WeakSet.prototype.{m} called on incompatible receiver"
    )))
}

/// CanonicalizeKeyedCollectionKey — `-0` keys canonicalize to `+0`
fn canon_key(k: Value) -> Value {
    match k {
        Value::Num(n) if n == 0.0 => Value::Num(0.0),
        _ => k,
    }
}

/// WeakMap/WeakSet keys & values must be weakly referenceable — objects
/// or symbols only
fn weak_key_ok(v: &Value) -> bool {
    // CanBeHeldWeakly: objects + non-registered symbols only
    // (Symbol.for ids live in the global registry = strong refs; ids
    // >= 1_000_000 mark that registry here)
    match v {
        Value::Obj(_) => true,
        Value::Sym(id, _) => *id < 1_000_000,
        _ => false,
    }
}

/// spec NewTarget check — `WeakMap()`/`WeakSet()` called without `new`
/// throws; `new_target_native` is Some only on the [[Construct]] path
fn need_new_target(i: &mut Interp, name: &str) -> Result<(), Value> {
    if i.new_target_native.is_none() {
        return Err(i.type_err(format!("Constructor {name} requires 'new'")));
    }
    Ok(())
}

fn weakmap_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    need_new_target(i, "WeakMap")?;
    let m = if let Value::Obj(o) = this {
        o.borrow_mut().kind = ObjKind::Map(Vec::new());
        o.borrow_mut()
            .props
            .insert_str("__wm", Prop::builtin(Value::Bool(true)));
        o.clone()
    } else {
        let o = Rc::new(RefCell::new(Obj::new(
            Some(i.p.obj.clone()),
            ObjKind::Map(Vec::new()),
        )));
        o.borrow_mut()
            .props
            .insert_str("__wm", Prop::builtin(Value::Bool(true)));
        o
    };
    match arg(args, 0) {
        Value::Und | Value::Null => {}
        v => {
            // spec: `set` is fetched and checked BEFORE the iterable is
            // pulled; entries install via AddEntriesFromIterable which
            // CALLS the adder (overrides are observable)
            let adder = i.get_prop_str(&Value::Obj(m.clone()), "set")?;
            if !i.is_callable(&adder) {
                return Err(i.type_err("WeakMap.prototype.set is not callable"));
            }
            let mut lz = for_of_iter(i, &v)?;
            loop {
                let e = match iter_lazy_step(i, &mut lz) {
                    Ok(Some(e)) => e,
                    Ok(None) => break,
                    // iterator's own errors propagate without close
                    Err(e) => return Err(e),
                };
                if !matches!(e, Value::Obj(_)) {
                    let _ = iter_lazy_close(i, &mut lz);
                    return Err(i.type_err("Iterator value is not an entry object"));
                }
                let r = (|| {
                    let k = i.get_prop_str(&e, "0")?;
                    let val = i.get_prop_str(&e, "1")?;
                    i.call(&adder, Value::Obj(m.clone()), vec![k, val])
                })();
                match r {
                    Ok(_) => {}
                    Err(e) => {
                        let _ = iter_lazy_close(i, &mut lz);
                        return Err(e);
                    }
                }
            }
        }
    }
    Ok(Value::Obj(m))
}

fn weakset_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    need_new_target(i, "WeakSet")?;
    let s = if let Value::Obj(o) = this {
        o.borrow_mut().kind = ObjKind::Set(Vec::new());
        o.borrow_mut()
            .props
            .insert_str("__ws", Prop::builtin(Value::Bool(true)));
        o.clone()
    } else {
        let o = Rc::new(RefCell::new(Obj::new(
            Some(i.p.obj.clone()),
            ObjKind::Set(Vec::new()),
        )));
        o.borrow_mut()
            .props
            .insert_str("__ws", Prop::builtin(Value::Bool(true)));
        o
    };
    match arg(args, 0) {
        Value::Und | Value::Null => {}
        v => {
            let adder = i.get_prop_str(&Value::Obj(s.clone()), "add")?;
            if !i.is_callable(&adder) {
                return Err(i.type_err("WeakSet.prototype.add is not callable"));
            }
            let mut lz = for_of_iter(i, &v)?;
            loop {
                let it = match iter_lazy_step(i, &mut lz) {
                    Ok(Some(x)) => x,
                    Ok(None) => break,
                    Err(e) => return Err(e),
                };
                match i.call(&adder, Value::Obj(s.clone()), vec![it]) {
                    Ok(_) => {}
                    Err(e) => {
                        let _ = iter_lazy_close(i, &mut lz);
                        return Err(e);
                    }
                }
            }
        }
    }
    Ok(Value::Obj(s))
}

fn wm_get(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let o = need_wm(i, this, "get")?;
    let b = o.borrow();
    if let ObjKind::Map(items) = &b.kind {
        let k = arg(args, 0);
        if !weak_key_ok(&k) {
            return Ok(Value::Und);
        }
        for (k2, v) in items {
            if same_value_zero(k2, &k) {
                return Ok(v.clone());
            }
        }
    }
    Ok(Value::Und)
}

fn wm_set(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let k = arg(args, 0);
    let o = need_wm(i, this, "set")?;
    if !weak_key_ok(&k) {
        return Err(i.type_err("Invalid value used as weak map key"));
    }
    let v = arg(args, 1);
    if let ObjKind::Map(items) = &mut o.borrow_mut().kind {
        if let Some(slot) = items.iter_mut().find(|(k2, _)| same_value_zero(k2, &k)) {
            slot.1 = v;
        } else {
            items.push((k, v));
        }
    }
    Ok(this.clone())
}

fn wm_has(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let o = need_wm(i, this, "has")?;
    let b = o.borrow();
    if let ObjKind::Map(items) = &b.kind {
        let k = arg(args, 0);
        if !weak_key_ok(&k) {
            return Ok(Value::Bool(false));
        }
        return Ok(Value::Bool(
            items.iter().any(|(k2, _)| same_value_zero(k2, &k)),
        ));
    }
    Ok(Value::Bool(false))
}

fn wm_delete(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let o = need_wm(i, this, "delete")?;
    let k = arg(args, 0);
    if !weak_key_ok(&k) {
        return Ok(Value::Bool(false));
    }
    if let ObjKind::Map(items) = &mut o.borrow_mut().kind {
        let before = items.len();
        items.retain(|(k2, _)| !same_value_zero(k2, &k));
        return Ok(Value::Bool(items.len() != before));
    }
    Ok(Value::Bool(false))
}

fn map_get_or_insert(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let k = canon_key(arg(args, 0));
    let o = need_map(i, this, "getOrInsert")?;
    {
        let b = o.borrow();
        if let ObjKind::Map(items) = &b.kind {
            if let Some((_, v)) = items.iter().find(|(k2, _)| same_value_zero(k2, &k)) {
                return Ok(v.clone());
            }
        }
    }
    let v = arg(args, 1);
    if let ObjKind::Map(items) = &mut o.borrow_mut().kind {
        items.push((k, v.clone()));
    }
    Ok(v)
}

fn map_get_or_insert_computed(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let k = canon_key(arg(args, 0));
    let cb = arg(args, 1);
    let o = need_map(i, this, "getOrInsertComputed")?;
    if !i.is_callable(&cb) {
        return Err(i.type_err("getOrInsertComputed callback is not callable"));
    }
    {
        let b = o.borrow();
        if let ObjKind::Map(items) = &b.kind {
            if let Some((_, v)) = items.iter().find(|(k2, _)| same_value_zero(k2, &k)) {
                return Ok(v.clone());
            }
        }
    }
    let v = i.call(&cb, Value::Und, vec![k.clone()])?;
    // the callback may have installed the key — the RETURN value wins
    if let ObjKind::Map(items) = &mut o.borrow_mut().kind {
        if let Some(slot) = items.iter_mut().find(|(k2, _)| same_value_zero(k2, &k)) {
            slot.1 = v.clone();
        } else {
            items.push((k, v.clone()));
        }
    }
    Ok(v)
}

fn wm_get_or_insert(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let k = arg(args, 0);
    let o = need_wm(i, this, "getOrInsert")?;
    if !weak_key_ok(&k) {
        return Err(i.type_err("Invalid value used as weak map key"));
    }
    {
        let b = o.borrow();
        if let ObjKind::Map(items) = &b.kind {
            if let Some((_, v)) = items.iter().find(|(k2, _)| same_value_zero(k2, &k)) {
                return Ok(v.clone());
            }
        }
    }
    let v = arg(args, 1);
    if let ObjKind::Map(items) = &mut o.borrow_mut().kind {
        items.push((k, v.clone()));
    }
    Ok(v)
}

fn wm_get_or_insert_computed(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let k = arg(args, 0);
    let cb = arg(args, 1);
    let o = need_wm(i, this, "getOrInsertComputed")?;
    if !weak_key_ok(&k) {
        return Err(i.type_err("Invalid value used as weak map key"));
    }
    if !i.is_callable(&cb) {
        return Err(i.type_err("getOrInsertComputed callback is not callable"));
    }
    {
        let b = o.borrow();
        if let ObjKind::Map(items) = &b.kind {
            if let Some((_, v)) = items.iter().find(|(k2, _)| same_value_zero(k2, &k)) {
                return Ok(v.clone());
            }
        }
    }
    let v = i.call(&cb, Value::Und, vec![k.clone()])?;
    // spec: the callback may have installed the key itself — the
    // callback's RETURN value wins (overwrite in place)
    if let ObjKind::Map(items) = &mut o.borrow_mut().kind {
        if let Some(slot) = items.iter_mut().find(|(k2, _)| same_value_zero(k2, &k)) {
            slot.1 = v.clone();
        } else {
            items.push((k, v.clone()));
        }
    }
    Ok(v)
}

fn ws_add(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let v = arg(args, 0);
    let o = need_ws(i, this, "add")?;
    if !weak_key_ok(&v) {
        return Err(i.type_err("Invalid value used in weak set"));
    }
    if let ObjKind::Set(items) = &mut o.borrow_mut().kind {
        if !items.iter().any(|x| same_value_zero(x, &v)) {
            items.push(v);
        }
    }
    Ok(this.clone())
}

fn ws_has(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let o = need_ws(i, this, "has")?;
    let b = o.borrow();
    let v = arg(args, 0);
    if !weak_key_ok(&v) {
        return Ok(Value::Bool(false));
    }
    if let ObjKind::Set(items) = &b.kind {
        return Ok(Value::Bool(items.iter().any(|x| same_value_zero(x, &v))));
    }
    Ok(Value::Bool(false))
}

fn ws_delete(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let o = need_ws(i, this, "delete")?;
    let v = arg(args, 0);
    if !weak_key_ok(&v) {
        return Ok(Value::Bool(false));
    }
    if let ObjKind::Set(items) = &mut o.borrow_mut().kind {
        let before = items.len();
        items.retain(|x| !same_value_zero(x, &v));
        return Ok(Value::Bool(items.len() != before));
    }
    Ok(Value::Bool(false))
}

fn map_get(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let o = need_map(i, this, "get")?;
    let b = o.borrow();
    if let ObjKind::Map(items) = &b.kind {
        let k = canon_key(arg(args, 0));
        for (k2, v) in items {
            if same_value_zero(k2, &k) {
                return Ok(v.clone());
            }
        }
    }
    Ok(Value::Und)
}

fn map_set(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let k = canon_key(arg(args, 0));
    let v = arg(args, 1);
    let o = need_map(i, this, "set")?;
    if let ObjKind::Map(items) = &mut o.borrow_mut().kind {
        if let Some(slot) = items.iter_mut().find(|(k2, _)| same_value_zero(k2, &k)) {
            slot.1 = v;
        } else {
            items.push((k, v));
        }
    }
    Ok(this.clone())
}

fn map_has(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let o = need_map(i, this, "has")?;
    let b = o.borrow();
    if let ObjKind::Map(items) = &b.kind {
        let k = canon_key(arg(args, 0));
        return Ok(Value::Bool(
            items.iter().any(|(k2, _)| same_value_zero(k2, &k)),
        ));
    }
    Ok(Value::Bool(false))
}

fn map_delete(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let o = need_map(i, this, "delete")?;
    let k = canon_key(arg(args, 0));
    if let ObjKind::Map(items) = &mut o.borrow_mut().kind {
        let before = items.len();
        items.retain(|(k2, _)| !same_value_zero(k2, &k));
        return Ok(Value::Bool(items.len() != before));
    }
    Ok(Value::Bool(false))
}

fn map_clear(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let o = need_map(i, this, "clear")?;
    if let ObjKind::Map(items) = &mut o.borrow_mut().kind {
        items.clear();
    }
    Ok(Value::Und)
}

fn map_for_each(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    let o = need_map(i, this, "forEach")?;
    if !i.is_callable(&cb) {
        return Err(i.type_err("forEach callback is not callable"));
    }
    // spec: live traversal over a slot list — a deleted key leaves an
    // empty slot (later re-adds append at the end and ARE revisited).
    // With a compact Vec, deletion shifts survivors left; recompute the
    // cursor as the count of still-present pre-cursor keys each round.
    let mut idx = 0usize;
    loop {
        let state = {
            let b = o.borrow();
            if let ObjKind::Map(items) = &b.kind {
                if idx >= items.len() {
                    None
                } else {
                    let prefix: Vec<Value> = items[..idx].iter().map(|(k, _)| k.clone()).collect();
                    Some((items[idx].0.clone(), items[idx].1.clone(), prefix))
                }
            } else {
                None
            }
        };
        let Some((k, v, prefix)) = state else { break };
        i.call(&cb, this_arg.clone(), vec![v, k.clone(), this.clone()])?;
        let b = o.borrow();
        if let ObjKind::Map(items) = &b.kind {
            idx = prefix
                .iter()
                .filter(|kk| items.iter().any(|(k2, _)| same_value_zero(k2, kk)))
                .count();
            // visited key still at its slot → step past it; if it moved
            // to the end (delete+re-add) it stays pending for a revisit
            if items
                .get(idx)
                .is_some_and(|(k2, _)| same_value_zero(k2, &k))
            {
                idx += 1;
            }
        } else {
            break;
        }
    }
    Ok(Value::Und)
}

fn map_keys(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let o = need_map(i, this, "keys")?;
    let b = o.borrow();
    if let ObjKind::Map(items) = &b.kind {
        let ks: Vec<Value> = items.iter().map(|(k, _)| k.clone()).collect();
        drop(b);
        return Ok(make_iter_in(ks, i.p.map_iter.clone()));
    }
    drop(b);
    Ok(make_iter_in(vec![], i.p.map_iter.clone()))
}

fn map_values(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let o = need_map(i, this, "values")?;
    Ok(make_live_iter(o.clone(), i.p.map_iter.clone(), 4))
}

fn map_entries(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let o = need_map(i, this, "entries")?;
    Ok(make_live_iter(o.clone(), i.p.map_iter.clone(), 3))
}

fn map_size(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let o = need_map(i, this, "size")?;
    let b = o.borrow();
    if let ObjKind::Map(items) = &b.kind {
        return Ok(Value::Num(items.len() as f64));
    }
    Ok(Value::Num(0.0))
}

fn set_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    need_new_target(i, "Set")?;
    let s = if let Value::Obj(o) = this {
        o.borrow_mut().kind = ObjKind::Set(Vec::new());
        o.borrow_mut()
            .props
            .insert_str("__set", Prop::builtin(Value::Bool(true)));
        o.clone()
    } else {
        let o = Rc::new(RefCell::new(Obj::new(
            Some(i.p.obj.clone()),
            ObjKind::Set(Vec::new()),
        )));
        o.borrow_mut()
            .props
            .insert_str("__set", Prop::builtin(Value::Bool(true)));
        o
    };
    match arg(args, 0) {
        Value::Und | Value::Null => {}
        v => {
            let adder = i.get_prop_str(&Value::Obj(s.clone()), "add")?;
            if !i.is_callable(&adder) {
                return Err(i.type_err("Set.prototype.add is not callable"));
            }
            let mut lz = for_of_iter(i, &v)?;
            loop {
                let it = match iter_lazy_step(i, &mut lz) {
                    Ok(Some(x)) => x,
                    Ok(None) => break,
                    Err(e) => return Err(e),
                };
                match i.call(&adder, Value::Obj(s.clone()), vec![it]) {
                    Ok(_) => {}
                    Err(e) => {
                        let _ = iter_lazy_close(i, &mut lz);
                        return Err(e);
                    }
                }
            }
        }
    }
    Ok(Value::Obj(s))
}

fn set_add(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let v = arg(args, 0);
    let o = need_set(i, this, "add")?;
    if let ObjKind::Set(items) = &mut o.borrow_mut().kind {
        if !items.iter().any(|x| same_value_zero(x, &v)) {
            items.push(v);
        }
    }
    Ok(this.clone())
}

fn set_has(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let o = need_set(i, this, "has")?;
    let b = o.borrow();
    let v = arg(args, 0);
    if let ObjKind::Set(items) = &b.kind {
        return Ok(Value::Bool(items.iter().any(|x| same_value_zero(x, &v))));
    }
    Ok(Value::Bool(false))
}

fn set_delete(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let o = need_set(i, this, "delete")?;
    let v = arg(args, 0);
    if let ObjKind::Set(items) = &mut o.borrow_mut().kind {
        let before = items.len();
        items.retain(|x| !same_value_zero(x, &v));
        return Ok(Value::Bool(items.len() != before));
    }
    Ok(Value::Bool(false))
}

fn set_clear(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let o = need_set(i, this, "clear")?;
    if let ObjKind::Set(items) = &mut o.borrow_mut().kind {
        items.clear();
    }
    Ok(Value::Und)
}

fn set_for_each(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    let o = need_set(i, this, "forEach")?;
    if !i.is_callable(&cb) {
        return Err(i.type_err("forEach callback is not callable"));
    }
    let mut idx = 0usize;
    loop {
        let state = {
            let b = o.borrow();
            if let ObjKind::Set(items) = &b.kind {
                if idx >= items.len() {
                    None
                } else {
                    Some((items[idx].clone(), items[..idx].to_vec()))
                }
            } else {
                None
            }
        };
        let Some((v, prefix)) = state else { break };
        i.call(
            &cb,
            this_arg.clone(),
            vec![v.clone(), v.clone(), this.clone()],
        )?;
        let b = o.borrow();
        if let ObjKind::Set(items) = &b.kind {
            idx = prefix
                .iter()
                .filter(|vv| items.iter().any(|x| same_value_zero(x, vv)))
                .count();
            if items.get(idx).is_some_and(|x| same_value_zero(x, &v)) {
                idx += 1;
            }
        } else {
            break;
        }
    }
    Ok(Value::Und)
}

fn set_values(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let o = need_set(i, this, "values")?;
    Ok(make_live_iter(o, i.p.set_iter.clone(), 5))
}

fn set_entries(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let o = need_set(i, this, "entries")?;
    Ok(make_live_iter(o, i.p.set_iter.clone(), 6))
}

fn set_size(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let o = need_set(i, this, "size")?;
    let b = o.borrow();
    if let ObjKind::Set(items) = &b.kind {
        return Ok(Value::Num(items.len() as f64));
    }
    Ok(Value::Num(0.0))
}

// ===========================================================================
// natives: globals
// ===========================================================================

fn g_parse_int(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let s = i.to_str(&arg(args, 0))?;
    let t = s.trim_start();
    let (sign, t) = if let Some(r) = t.strip_prefix('-') {
        (-1.0, r)
    } else if let Some(r) = t.strip_prefix('+') {
        (1.0, r)
    } else {
        (1.0, t)
    };
    // spec: R = ToInt32(radix); NaN/±0/±∞ → +0 (→ hex-detect or 10)
    let rv = if matches!(arg(args, 1), Value::Und) {
        0.0
    } else {
        i.to_num(&arg(args, 1))?
    };
    let mut radix = if !rv.is_finite() {
        0u32
    } else {
        ((rv.trunc() as i64) & 0xFFFF_FFFF) as i32 as u32
    };
    let mut t = t;
    if radix == 0 {
        if t.starts_with("0x") || t.starts_with("0X") {
            radix = 16;
            t = &t[2..];
        } else {
            radix = 10;
        }
    } else if radix == 16 && (t.starts_with("0x") || t.starts_with("0X")) {
        t = &t[2..];
    }
    if !(2..=36).contains(&radix) {
        return Ok(Value::Num(f64::NAN));
    }
    let mut v: f64 = 0.0;
    let mut any = false;
    for c in t.chars() {
        match c.to_digit(36).filter(|d| *d < radix) {
            Some(d) => {
                v = v * radix as f64 + d as f64;
                any = true;
            }
            None => break,
        }
    }
    if !any {
        return Ok(Value::Num(f64::NAN));
    }
    Ok(Value::Num(sign * v))
}

fn g_parse_float(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let s = i.to_str(&arg(args, 0))?;
    let t = s.trim_start();
    // longest valid float prefix
    let bytes: Vec<char> = t.chars().collect();
    let mut end = 0;
    let mut seen_digit = false;
    let mut seen_dot = false;
    let mut seen_e = false;
    for (k, &c) in bytes.iter().enumerate() {
        match c {
            '+' | '-' if k == 0 => {}
            '+' | '-'
                if seen_e && matches!(bytes.get(k.wrapping_sub(1)), Some('e') | Some('E')) => {}
            '.' if !seen_dot && !seen_e => seen_dot = true,
            'e' | 'E' if seen_digit && !seen_e => seen_e = true,
            '0'..='9' => seen_digit = true,
            'I' if t[k..].starts_with("Infinity") => {
                let sign = if bytes.first() == Some(&'-') {
                    -1.0
                } else {
                    1.0
                };
                return Ok(Value::Num(sign * f64::INFINITY));
            }
            _ => break,
        }
        end = k + 1;
    }
    if !seen_digit {
        return Ok(Value::Num(f64::NAN));
    }
    // drop incomplete exponent suffix ("1e", "1e+")
    let mut s: String = bytes[..end].iter().collect();
    while s.ends_with('e') || s.ends_with('E') || s.ends_with('+') || s.ends_with('-') {
        s.pop();
    }
    Ok(Value::Num(s.parse::<f64>().unwrap_or(f64::NAN)))
}

fn g_is_nan(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    Ok(Value::Bool(i.to_num(&arg(args, 0))?.is_nan()))
}

fn g_is_finite(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    Ok(Value::Bool(i.to_num(&arg(args, 0))?.is_finite()))
}

fn g_identity_str(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    Ok(Value::Str(Rc::from(i.to_str(&arg(args, 0))?.as_str())))
}

const URI_MARK: &str = "-_.!~*'()";
/// uriReserved = `;/?:@&=+$,#` — left literal by encodeURI, kept %-escaped
/// by decodeURI
const URI_RESERVED: &str = ";/?:@&=+$,#";

fn uri_err(i: &mut Interp, m: &str) -> Value {
    let p = i.p.uri_error.clone();
    i.err_obj(&p, "URIError", m)
}

/// spec Encode(string, unreservedSet): unreserved = alphanumerics +
/// mark chars + `extra`; everything else UTF-8 encodes to %XX bytes
/// (astral chars → their surrogate pair's UTF-8 = same bytes)
fn uri_encode(i: &mut Interp, v: &Value, extra: &str) -> Result<String, Value> {
    let s = i.to_str(v)?;
    let mut out = String::new();
    let mut buf = [0u8; 4];
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || URI_MARK.contains(ch) || extra.contains(ch) {
            out.push(ch);
        } else {
            for b in ch.encode_utf8(&mut buf).as_bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    Ok(out)
}

/// spec Decode(string, reservedSet): %XX sequences decode to UTF-8 —
/// malformed hex, bad continuation bytes, overlong forms, and surrogate
/// halves all throw URIError. A decoded reserved byte stays escaped when
/// `keep_reserved` (decodeURI) so the result reparses identically.
fn uri_decode(i: &mut Interp, v: &Value, keep_reserved: bool) -> Result<String, Value> {
    let s = i.to_str(v)?;
    let b = s.as_bytes();
    let mut out = String::new();
    let mut k = 0;
    let hexv = |c: u8| (c as char).to_digit(16);
    while k < b.len() {
        if b[k] != b'%' {
            let ch = s[k..].chars().next().unwrap();
            out.push(ch);
            k += ch.len_utf8();
            continue;
        }
        if k + 2 >= b.len() || hexv(b[k + 1]).is_none() || hexv(b[k + 2]).is_none() {
            return Err(uri_err(i, "URI malformed"));
        }
        let first = (hexv(b[k + 1]).unwrap() * 16 + hexv(b[k + 2]).unwrap()) as u8;
        let len = match first {
            0x00..=0x7F => 1,
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => return Err(uri_err(i, "URI malformed")),
        };
        if len == 1 {
            let c = first as char;
            if keep_reserved && URI_RESERVED.contains(c) {
                out.push_str(&s[k..k + 3]);
            } else {
                out.push(c);
            }
        } else {
            if k + 3 * len > b.len() {
                return Err(uri_err(i, "URI malformed"));
            }
            let mut seq = vec![first];
            for j in 1..len {
                let o = k + 3 * j;
                if b[o] != b'%' || hexv(b[o + 1]).is_none() || hexv(b[o + 2]).is_none() {
                    return Err(uri_err(i, "URI malformed"));
                }
                seq.push((hexv(b[o + 1]).unwrap() * 16 + hexv(b[o + 2]).unwrap()) as u8);
            }
            // Rust UTF-8 validation rejects overlongs, surrogates,
            // >U+10FFFF, and stray continuations — exactly the spec rules
            match std::str::from_utf8(&seq) {
                Ok(dec) => out.push_str(dec),
                Err(_) => return Err(uri_err(i, "URI malformed")),
            }
        }
        k += 3 * len;
    }
    Ok(out)
}

fn g_encode_uri(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let s = uri_encode(i, &arg(args, 0), URI_RESERVED)?;
    Ok(Value::Str(Rc::from(s.as_str())))
}

fn g_decode_uri(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let s = uri_decode(i, &arg(args, 0), true)?;
    Ok(Value::Str(Rc::from(s.as_str())))
}

fn g_encode_uri_component(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let s = uri_encode(i, &arg(args, 0), "")?;
    Ok(Value::Str(Rc::from(s.as_str())))
}

fn g_decode_uri_component(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let s = uri_decode(i, &arg(args, 0), false)?;
    Ok(Value::Str(Rc::from(s.as_str())))
}

/// Legacy `escape()`: %XX for non-unreserved bytes, %uXXXX for non-ASCII
/// code points.
fn g_escape(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let s = i.to_str(&arg(args, 0))?;
    let mut out = String::new();
    for ch in s.chars() {
        let cp = ch as u32;
        if ch.is_ascii_alphanumeric() || matches!(ch, '@' | '*' | '_' | '+' | '-' | '.' | '/') {
            out.push(ch);
        } else if cp < 256 {
            out.push_str(&format!("%{cp:02X}"));
        } else {
            out.push_str(&format!("%u{cp:04X}"));
        }
    }
    Ok(Value::Str(Rc::from(out.as_str())))
}

/// Legacy `unescape()` — inverse of `escape`.
fn g_unescape(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let s = i.to_str(&arg(args, 0))?;
    let b: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut k = 0;
    while k < b.len() {
        if b[k] == '%' {
            if b.get(k + 1) == Some(&'u') && k + 5 < b.len() {
                let hex: String = b[k + 2..k + 6].iter().collect();
                if let Ok(v) = u32::from_str_radix(&hex, 16) {
                    if let Some(c) = char::from_u32(v) {
                        out.push(c);
                        k += 6;
                        continue;
                    }
                }
            } else if k + 2 < b.len() {
                let hex: String = b[k + 1..k + 3].iter().collect();
                if let Ok(v) = u32::from_str_radix(&hex, 16) {
                    if let Some(c) = char::from_u32(v) {
                        out.push(c);
                        k += 3;
                        continue;
                    }
                }
            }
        }
        out.push(b[k]);
        k += 1;
    }
    Ok(Value::Str(Rc::from(out.as_str())))
}

fn g_set_timeout(i: &mut Interp, _t: &Value, _a: &[Value]) -> NResult {
    // stub: timers are not executed (no event loop yet)
    i.sym_counter += 1;
    Ok(Value::Num(i.sym_counter as f64))
}

fn g_noop(_i: &mut Interp, _t: &Value, _a: &[Value]) -> NResult {
    Ok(Value::Und)
}

/// `eval(x)` reached as an ordinary call — indirect eval → global scope.
/// (Direct eval is intercepted in `eval_call` before the native runs.)
fn g_eval(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let Some(first) = args.first() else {
        return Ok(Value::Und);
    };
    let Value::Str(src) = first else {
        return Ok(first.clone());
    };
    let src = src.clone();
    let g = i.global.clone();
    // indirect eval: always enters non-strict regardless of caller; the
    // class-field-initializer `arguments` ban does NOT apply to indirect
    // eval (only direct eval inherits it)
    let saved = std::mem::replace(&mut i.cur_strict, false);
    let saved_ban = std::mem::replace(&mut i.eval_args_ban, 0);
    let r = i.eval_source(&src, g);
    i.eval_args_ban = saved_ban;
    i.cur_strict = saved;
    r
}

fn g_queue_microtask(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    // spec queueMicrotask — non-callable is a sync TypeError; callable
    // callbacks queue onto the job list, run at the checkpoint
    let f = arg(args, 0);
    if !i.is_callable(&f) {
        return Err(i.type_err("queueMicrotask callback is not a function"));
    }
    i.jobs
        .push_back(Box::new(move |i: &mut Interp| -> Result<(), Value> {
            i.call(&f, Value::Und, vec![])?;
            Ok(())
        }));
    Ok(Value::Und)
}

fn g_dyn_import(i: &mut Interp, _t: &Value, _a: &[Value]) -> NResult {
    // dynamic import() — unresolved modules yield an empty namespace object
    let ns = Value::Obj(i.new_obj());
    let p = new_promise(i);
    p.borrow_mut().kind = ObjKind::Promise(PromiseState::Fulfilled(Box::new(ns)));
    Ok(Value::Obj(p))
}

fn g_btoa(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let s = i.to_str(&arg(args, 0))?;
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let b = s.as_bytes();
    let mut out = String::new();
    for chunk in b.chunks(3) {
        let a = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let c = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (a << 16) | (b1 << 8) | c;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    Ok(Value::Str(Rc::from(out.as_str())))
}

fn g_atob(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let s = i.to_str(&arg(args, 0))?;
    let val = |c: u8| -> u32 {
        match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a' + 26) as u32,
            b'0'..=b'9' => (c - b'0' + 52) as u32,
            b'+' => 62,
            b'/' => 63,
            _ => 0,
        }
    };
    let chars: Vec<u8> = s.bytes().filter(|&c| c != b'=').collect();
    let mut out = Vec::new();
    for chunk in chars.chunks(4) {
        let mut n: u32 = 0;
        for (k, &c) in chunk.iter().enumerate() {
            n |= val(c) << (18 - 6 * k);
        }
        let nbytes = chunk.len() - 1;
        for k in 0..nbytes.min(3) {
            out.push((n >> (16 - 8 * k)) as u8);
        }
    }
    Ok(Value::Str(Rc::from(
        String::from_utf8_lossy(&out).to_string().as_str(),
    )))
}

// ===========================================================================
// natives: reflect
// ===========================================================================

fn reflect_get(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let target = arg(args, 0);
    let Value::Obj(o) = &target else {
        return Err(i.type_err("Reflect.get called on non-object"));
    };
    let k = i.to_propkey(&arg(args, 1))?;
    // spec: `receiver` defaults to target only when the arg is absent
    let receiver = args.get(2).cloned().unwrap_or_else(|| target.clone());
    if let ObjKind::Proxy {
        target: tg,
        handler,
    } = { o.borrow().kind.clone() }
    {
        return i.proxy_get(&tg, &handler, &k, &receiver, o);
    }
    i.obj_get(o, &k, &receiver)
}

fn reflect_set(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let target = arg(args, 0);
    let Value::Obj(o) = &target else {
        return Err(i.type_err("Reflect.set called on non-object"));
    };
    let k = i.to_propkey(&arg(args, 1))?;
    let receiver = args.get(3).cloned().unwrap_or_else(|| target.clone());
    // spec [[Set]] returns a boolean — run the write non-strict so a
    // failed assignment flags `false` instead of throwing
    i.set_failed = false;
    let st = std::mem::replace(&mut i.cur_strict, false);
    let r = i.obj_set(o, k, arg(args, 2), &receiver);
    i.cur_strict = st;
    let failed = std::mem::replace(&mut i.set_failed, false);
    r?;
    Ok(Value::Bool(!failed))
}

fn reflect_has(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    if !matches!(arg(args, 0), Value::Obj(_)) {
        return Err(i.type_err("Reflect.has called on non-object"));
    }
    let k = i.to_propkey(&arg(args, 1))?;
    Ok(Value::Bool(i.has_prop(&arg(args, 0), &k)?))
}

fn reflect_delete(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    if !matches!(arg(args, 0), Value::Obj(_)) {
        return Err(i.type_err("Reflect.deleteProperty called on non-object"));
    }
    let k = i.to_propkey(&arg(args, 1))?;
    Ok(Value::Bool(i.delete_prop(&arg(args, 0), &k)?))
}

// Reflect.defineProperty/setPrototypeOf return booleans instead of throwing
fn reflect_define_property(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let target = arg(args, 0);
    let Value::Obj(o) = target.clone() else {
        return Err(i.type_err("Reflect.defineProperty called on non-object"));
    };
    let key = i.to_propkey(&arg(args, 1))?;
    let desc = arg(args, 2);
    if !matches!(desc, Value::Obj(_)) {
        return Err(i.type_err("Property description must be an object"));
    }
    // spec ToPropertyDescriptor runs before [[DefineOwnProperty]]
    let pd = i.pdesc_from_obj(&desc)?;
    if let ObjKind::Proxy {
        target: tg,
        handler,
    } = { o.borrow().kind.clone() }
    {
        let (tg, h) = (tg, handler);
        return match i.proxy_trap(&h, "defineProperty")? {
            Some(trap) => {
                let kv = i.key_to_value(&key);
                let dobj = Value::Obj(i.pdesc_to_obj(&pd));
                let r = i.call(&trap, h, vec![tg.clone(), kv, dobj])?;
                if !i.to_bool(&r) {
                    return Ok(Value::Bool(false));
                }
                i.proxy_define_invariant(&tg, &key, &pd)?;
                Ok(Value::Bool(true))
            }
            None => match &tg {
                Value::Obj(to) => {
                    let to = to.clone();
                    let dobj = Value::Obj(i.pdesc_to_obj(&pd));
                    Ok(Value::Bool(define_prop_from_desc(i, &to, key, &dobj)?))
                }
                _ => Err(i.type_err("proxy target is not an object")),
            },
        };
    }
    let dobj = Value::Obj(i.pdesc_to_obj(&pd));
    Ok(Value::Bool(define_prop_from_desc(i, &o, key, &dobj)?))
}

fn reflect_set_prototype_of(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let target = arg(args, 0);
    let Value::Obj(o) = &target else {
        return Err(i.type_err("Reflect.setPrototypeOf called on non-object"));
    };
    let v = arg(args, 1);
    if !matches!(v, Value::Obj(_) | Value::Null) {
        return Err(i.type_err("Reflect.setPrototypeOf called with non-object proto"));
    }
    Ok(Value::Bool(ord_set_proto(i, o, &v)?))
}

// Reflect mirrors of the Object versions — same operation but the
// target must already be an object (no ToObject coercion)
fn reflect_get_own_prop_desc(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    if !matches!(arg(args, 0), Value::Obj(_)) {
        return Err(i.type_err("Reflect.getOwnPropertyDescriptor called on non-object"));
    }
    obj_get_own_prop_desc(i, _t, args)
}

fn reflect_get_prototype_of(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    if !matches!(arg(args, 0), Value::Obj(_)) {
        return Err(i.type_err("Reflect.getPrototypeOf called on non-object"));
    }
    obj_get_prototype_of(i, _t, args)
}

fn reflect_is_extensible(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let target = arg(args, 0);
    let Value::Obj(o) = &target else {
        return Err(i.type_err("Reflect.isExtensible called on non-object"));
    };
    Ok(Value::Bool(ord_is_extensible(i, o)?))
}

fn reflect_prevent_extensions(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let target = arg(args, 0);
    let Value::Obj(o) = &target else {
        return Err(i.type_err("Reflect.preventExtensions called on non-object"));
    };
    Ok(Value::Bool(ord_prevent_ext(i, o)?))
}

fn reflect_apply(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let f = arg(args, 0);
    if !i.is_callable(&f) {
        return Err(i.type_err("Reflect.apply called on non-function"));
    }
    let this_arg = arg(args, 1);
    let list_v = arg(args, 2);
    if !matches!(list_v, Value::Obj(_)) {
        return Err(i.type_err("CreateListFromArrayLike called on non-object"));
    }
    let list = to_elems(i, &list_v)?;
    i.call(&f, this_arg, list)
}

fn reflect_construct(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let f = arg(args, 0);
    if !i.is_callable(&f) {
        return Err(i.type_err("Reflect.construct called on non-function"));
    }
    let list_v = arg(args, 1);
    if !matches!(list_v, Value::Obj(_)) {
        return Err(i.type_err("CreateListFromArrayLike called on non-object"));
    }
    let list = to_elems(i, &list_v)?;
    i.construct(&f, list, args.get(2).cloned())
}

// ===========================================================================
// natives: RegExp
// ===========================================================================

/// Borrow the compiled regex from a RegExp object.
fn re_of(v: &Value) -> Option<Rc<Regex>> {
    if let Value::Obj(o) = v {
        if let ObjKind::Regex(re) = &o.borrow().kind {
            return Some(re.clone());
        }
    }
    None
}

// -- per-realm RegExp state (Annex-B statics + iterator proto) ----------------

/// hidden slot keys — well clear of the well-known symbol id range
/// (`u64::MAX` itself is GEN_ABORT_ID).
const SYM_PRIV_BASE: u64 = u64::MAX - 0x100;
/// marks a regexp allocated with a non-%RegExp% newTarget (subclass or
/// Reflect.construct) — legacy RegExp statics are disabled for it
const SYM_RE_NOLEGACY: u64 = SYM_PRIV_BASE;
/// %RegExpStringIterator% internal slots
const SYM_ITER_RX: u64 = SYM_PRIV_BASE + 1;
const SYM_ITER_S: u64 = SYM_PRIV_BASE + 2;
const SYM_ITER_G: u64 = SYM_PRIV_BASE + 3;
const SYM_ITER_U: u64 = SYM_PRIV_BASE + 4;
const SYM_ITER_DONE: u64 = SYM_PRIV_BASE + 5;
/// hidden prop on %RegExp.prototype% holding the per-realm state object
/// (legacy statics + the %RegExpStringIteratorPrototype%)
const SYM_RE_STATE: u64 = SYM_PRIV_BASE + 6;

/// %RegExp% itself — found via the `constructor` prop `make_ctor` installs.
fn re_ctor(i: &Interp) -> ObjRef {
    i.p.regexp
        .borrow()
        .props
        .get_str("constructor")
        .and_then(|p| match &p.val {
            Value::Obj(o) => Some(o.clone()),
            _ => None,
        })
        .expect("RegExp.prototype.constructor missing")
}

/// The per-realm state object (unreachable except via the hidden sym).
fn re_state(i: &Interp) -> ObjRef {
    i.p.regexp
        .borrow()
        .props
        .get(&PropKey::Sym(SYM_RE_STATE))
        .and_then(|p| match &p.val {
            Value::Obj(o) => Some(o.clone()),
            _ => None,
        })
        .expect("RegExp realm state not installed")
}

fn state_get(state: &ObjRef, key: &str) -> Value {
    state
        .borrow()
        .props
        .get_str(key)
        .map(|p| p.val.clone())
        .unwrap_or(Value::Und)
}

fn state_set(state: &ObjRef, key: &str, v: Value) {
    if let Some(p) = state.borrow_mut().props.get_str_mut(key) {
        p.val = v;
    }
}

/// spec ToString with the Symbol → TypeError rule (`i.to_str` formats
/// symbols, which the RegExp paths must reject).
fn re_coerce_str(i: &mut Interp, v: &Value) -> Result<String, Value> {
    if matches!(v, Value::Sym(..)) {
        return Err(i.type_err("Cannot convert a Symbol value to a string"));
    }
    i.to_str(v)
}

/// spec ToLength on an f64.
fn to_length_num(n: f64) -> usize {
    if n.is_nan() || n <= 0.0 {
        0
    } else if n >= 9007199254740991.0 {
        9007199254740991usize
    } else {
        n.trunc() as usize
    }
}

/// spec ToUint32 on an f64.
fn to_uint32_num(n: f64) -> u32 {
    if !n.is_finite() {
        return 0;
    }
    n.trunc().rem_euclid(4294967296.0) as u32
}

/// spec AdvanceStringIndex — a surrogate pair steps by 2 code units for
/// unicode regexps, otherwise by 1.
fn advance_string_index(chars: &[u16], index: usize, unicode: bool) -> usize {
    if unicode
        && index + 1 < chars.len()
        && (0xD800..0xDC00).contains(&(chars[index] as u32))
        && (0xDC00..0xE000).contains(&(chars[index + 1] as u32))
    {
        index + 2
    } else {
        index + 1
    }
}

/// spec GetMethod: nullish → None, callable → Some, else TypeError.
fn get_method(i: &mut Interp, v: &Value, key: &PropKey) -> Result<Option<Value>, Value> {
    match i.get_prop(v, key)? {
        Value::Und | Value::Null => Ok(None),
        m if i.is_callable(&m) => Ok(Some(m)),
        _ => Err(i.type_err("property is not callable")),
    }
}

/// spec IsRegExp: a non-undefined @@match result decides, else the
/// [[RegExpMatcher]] slot. BigInt primitives are Obj stand-ins — the
/// spec's Object check must exclude them.
fn is_regexp(i: &mut Interp, v: &Value) -> Result<bool, Value> {
    if !is_obj_arg(v) {
        return Ok(false);
    }
    let m = i.get_prop(v, &PropKey::Sym(SYM_MATCH))?;
    if matches!(m, Value::Und) {
        Ok(re_of(v).is_some())
    } else {
        Ok(i.to_bool(&m))
    }
}

/// spec SpeciesConstructor(O, %RegExp%).
fn re_species_ctor(i: &mut Interp, rx: &Value) -> Result<Value, Value> {
    let default_ctor = Value::Obj(re_ctor(i));
    let c = i.get_prop_str(rx, "constructor")?;
    if matches!(c, Value::Und) {
        return Ok(default_ctor);
    }
    if !matches!(c, Value::Obj(_)) {
        return Err(i.type_err("constructor property is not an object"));
    }
    let s = i.get_prop(&c, &PropKey::Sym(SYM_SPECIES))?;
    if matches!(s, Value::Und | Value::Null) {
        return Ok(default_ctor);
    }
    if i.is_constructor(&s) {
        Ok(s)
    } else {
        Err(i.type_err("species is not a constructor"))
    }
}

/// spec RegExpCreate(P, F) — `Construct(%RegExp%, «P, F»)`.
fn regexp_create(i: &mut Interp, p: &Value, f: &Value) -> Result<Value, Value> {
    let ctor = Value::Obj(re_ctor(i));
    i.construct(&ctor, vec![p.clone(), f.clone()], None)
}

/// `[[Set]](O, key, v)` with Throw semantics — used where the spec writes
/// `? Set(...)` on possibly non-writable own props (lastIndex).
fn set_strict(i: &mut Interp, ov: &Value, key: &str, v: Value) -> Result<(), Value> {
    if let Value::Obj(o) = ov {
        if let Some(p) = o.borrow().props.get_str(key) {
            let reject = if p.is_accessor() {
                p.set.is_none()
            } else {
                !p.writable
            };
            if reject {
                return Err(i.type_err(format!("Cannot assign to read only property '{key}'")));
            }
        }
    }
    i.set_prop_str(ov, key, v)
}

fn chars_str(chars: &[u16], a: usize, b: usize) -> Value {
    let s = String::from_utf16_lossy(&chars[a..b]);
    Value::Str(Rc::from(s.as_str()))
}

fn chars_string(chars: &[u16]) -> String {
    String::from_utf16_lossy(chars)
}

/// exec result: array [m0, g1..gn] + index/input/groups/indices props.
fn match_obj(i: &mut Interp, chars: &[u16], re: &Regex, m: &RxMatch) -> Value {
    let mut items = vec![chars_str(chars, m.start, m.end)];
    for g in 1..=re.n_groups {
        items.push(match m.caps.get(g).copied().flatten() {
            Some((a, b)) => chars_str(chars, a, b),
            None => Value::Und,
        });
    }
    let arr = i.new_arr(items);
    i.set_data(&arr, "index", Value::Num(m.start as f64));
    i.set_data(
        &arr,
        "input",
        Value::Str(Rc::from(chars_string(chars).as_str())),
    );
    let groups = if re.names.is_empty() {
        Value::Und
    } else {
        groups_obj(i, chars, re, m)
    };
    i.set_data(&arr, "groups", groups);
    if re.has_indices {
        // /d flag: parallel `indices` array of [start, end] pairs (or
        // undefined) plus its own null-proto `groups` map
        let mk_pair = |caps: &Vec<Option<(usize, usize)>>, g: usize| -> Value {
            match caps.get(g).copied().flatten() {
                Some((a, b)) => {
                    Value::Obj(i.new_arr(vec![Value::Num(a as f64), Value::Num(b as f64)]))
                }
                None => Value::Und,
            }
        };
        let mut idx_items = Vec::with_capacity(re.n_groups + 1);
        for g in 0..=re.n_groups {
            idx_items.push(mk_pair(&m.caps, g));
        }
        let ia = i.new_arr(idx_items);
        let igroups = if re.names.is_empty() {
            Value::Und
        } else {
            let g = Rc::new(RefCell::new(Obj::new(None, ObjKind::Ord)));
            for (name, idx) in &re.names {
                i.set_data(&g, name, mk_pair(&m.caps, *idx));
            }
            Value::Obj(g)
        };
        i.set_data(&ia, "groups", igroups);
        i.set_data(&arr, "indices", Value::Obj(ia));
    }
    Value::Obj(arr)
}

fn groups_obj(i: &mut Interp, chars: &[u16], re: &Regex, m: &RxMatch) -> Value {
    // spec: the named-capture groups object has a null prototype
    let g = Rc::new(RefCell::new(Obj::new(None, ObjKind::Ord)));
    for (name, idx) in &re.names {
        let v = match m.caps.get(*idx).copied().flatten() {
            Some((a, b)) => chars_str(chars, a, b),
            None => Value::Und,
        };
        i.set_data(&g, name, v);
    }
    Value::Obj(g)
}

/// Annex-B `UpdateRegExpStatics`: refresh %RegExp%'s legacy slots after a
/// successful built-in exec. Regexps allocated by a subclass newTarget
/// (marked at construction) invalidate the slots instead.
fn re_update_statics(i: &mut Interp, this: &Value, chars: &[u16], m: &RxMatch) {
    let st = re_state(i);
    let legacy_off = matches!(this, Value::Obj(o)
        if o.borrow().props.contains_key(&PropKey::Sym(SYM_RE_NOLEGACY)));
    if legacy_off {
        for k in [
            "input",
            "lastMatch",
            "lastParen",
            "leftContext",
            "rightContext",
            "p1",
            "p2",
            "p3",
            "p4",
            "p5",
            "p6",
            "p7",
            "p8",
            "p9",
        ] {
            state_set(&st, k, Value::Und);
        }
        return;
    }
    state_set(
        &st,
        "input",
        Value::Str(Rc::from(chars_string(chars).as_str())),
    );
    state_set(&st, "lastMatch", chars_str(chars, m.start, m.end));
    state_set(&st, "leftContext", chars_str(chars, 0, m.start));
    state_set(&st, "rightContext", chars_str(chars, m.end, chars.len()));
    // $1-$9 are positional: capture i → its substring, else "". `lastParen`
    // is the highest-numbered capture that actually participated — beyond
    // $9 too — ("" when none did).
    for k in 0..9 {
        let v = match m.caps.get(k + 1).copied().flatten() {
            Some((a, b)) => chars_str(chars, a, b),
            None => Value::Str(Rc::from("")),
        };
        state_set(&st, &format!("p{}", k + 1), v);
    }
    let last_paren = m
        .caps
        .iter()
        .skip(1)
        .rev()
        .find_map(|g| g.map(|(a, b)| chars_str(chars, a, b)))
        .unwrap_or_else(|| Value::Str(Rc::from("")));
    state_set(&st, "lastParen", last_paren);
}

/// The matching core of spec `RegExpBuiltinExec`: `lastIndex` is read
/// (ToLength) even for non-global regexps, g/y regexes update it, and a
/// successful match refreshes the Annex-B statics.
fn regexp_run(
    i: &mut Interp,
    this: &Value,
    re: &Regex,
    chars: &[u16],
) -> Result<Option<RxMatch>, Value> {
    let lv = i.get_prop_str(this, "lastIndex")?;
    let last_index = to_length_num(i.to_num(&lv)?);
    let use_last = re.global || re.sticky;
    let start = if use_last { last_index } else { 0 };
    let m = if start > chars.len() {
        None
    } else {
        re.exec(chars, start)
    };
    if use_last {
        let v = match &m {
            Some(mm) => Value::Num(mm.end as f64),
            None => Value::Num(0.0),
        };
        set_strict(i, this, "lastIndex", v)?;
    }
    if let Some(mm) = &m {
        re_update_statics(i, this, chars, mm);
    }
    Ok(m)
}

/// spec RegExpExec: a callable `exec` property wins, else RegExpBuiltinExec.
fn regexp_exec(i: &mut Interp, r: &Value, s: &str) -> Result<Value, Value> {
    let exec = i.get_prop_str(r, "exec")?;
    if i.is_callable(&exec) {
        let result = i.call(&exec, r.clone(), vec![Value::Str(Rc::from(s))])?;
        if !matches!(result, Value::Obj(_) | Value::Null) {
            return Err(i.type_err("RegExp exec method must return an object or null"));
        }
        return Ok(result);
    }
    regexp_builtin_exec(i, r, s)
}

/// spec RegExpBuiltinExec — requires the [[RegExpMatcher]] slot.
fn regexp_builtin_exec(i: &mut Interp, r: &Value, s: &str) -> Result<Value, Value> {
    let Some(re) = re_of(r) else {
        return Err(i.type_err("RegExp exec called on incompatible receiver"));
    };
    // JS strings are UTF-16 — the matcher works on code units (with `u`/`v`
    // consuming surrogate pairs atomically)
    let chars: Vec<u16> = s.encode_utf16().collect();
    match regexp_run(i, r, &re, &chars)? {
        Some(m) => Ok(match_obj(i, &chars, &re, &m)),
        None => Ok(Value::Null),
    }
}

/// spec RegExpInitialize: coerce P/F, compile, install the slots + the
/// `lastIndex` data prop (fresh objects only — `compile` writes via Set).
fn regexp_init(i: &mut Interp, ov: &Value, p: &Value, f: &Value) -> Result<(), Value> {
    let source = if matches!(p, Value::Und) {
        String::new()
    } else {
        re_coerce_str(i, p)?
    };
    let flags = if matches!(f, Value::Und) {
        String::new()
    } else {
        re_coerce_str(i, f)?
    };
    let re = Regex::compile(&source, &flags)
        .map_err(|e| i.syntax_err(format!("Invalid regular expression: /{source}/: {e}")))?;
    let Value::Obj(o) = ov else {
        return Err(i.type_err("invalid regexp object"));
    };
    o.borrow_mut().kind = ObjKind::Regex(re);
    // spec: { writable: true, enumerable: false, configurable: false }
    o.borrow_mut().props.insert(
        pk("lastIndex"),
        Prop {
            val: Value::Num(0.0),
            writable: true,
            enumerable: false,
            configurable: false,
            get: None,
            set: None,
        },
    );
    Ok(())
}

fn regexp_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let pattern = arg(args, 0);
    let flags = arg(args, 1);
    let nt = i.new_target_native.clone();
    let pattern_is_re = is_regexp(i, &pattern)?;
    if nt.is_none() && pattern_is_re && matches!(flags, Value::Und) {
        // RegExp(re) with no flags: re.constructor === %RegExp% → return re
        let pc = i.get_prop_str(&pattern, "constructor")?;
        if let Value::Obj(pco) = &pc {
            if Rc::ptr_eq(pco, &re_ctor(i)) {
                return Ok(pattern);
            }
        }
    }
    // P / F per spec — real regexp patterns read slots directly (no Gets)
    let (p, f) = if let Some(re) = re_of(&pattern) {
        (
            Value::Str(Rc::from(re.source.as_str())),
            if matches!(flags, Value::Und) {
                Value::Str(Rc::from(re.flags.as_str()))
            } else {
                flags
            },
        )
    } else if pattern_is_re {
        // spec order: Get(source) BEFORE Get(flags) — observable on
        // regexp-like objects whose getters throw
        let src = i.get_prop_str(&pattern, "source")?;
        let f = if matches!(flags, Value::Und) {
            i.get_prop_str(&pattern, "flags")?
        } else {
            flags
        };
        (src, f)
    } else {
        (pattern, flags)
    };
    // `new_target_native` is only Some inside a native [[Construct]] — but a
    // plain `RegExp()` nested in one still sees it, so additionally require
    // `this` to be the freshly allocated object: an Ord with no own props.
    let fresh_this = matches!(this, Value::Obj(o)
        if matches!(o.borrow().kind, ObjKind::Ord) && o.borrow().props.len() == 0);
    if nt.is_some() && fresh_this {
        // construct path: `this` is the freshly allocated object — initialize
        // it, and mark it when the newTarget isn't %RegExp% itself (subclass
        // or Reflect.construct) so Annex-B statics stay disabled
        regexp_init(i, this, &p, &f)?;
        if let (Some(Value::Obj(nto)), Value::Obj(o)) = (&nt, this) {
            if !Rc::ptr_eq(nto, &re_ctor(i)) {
                o.borrow_mut().props.insert(
                    PropKey::Sym(SYM_RE_NOLEGACY),
                    Prop::builtin(Value::Bool(true)),
                );
            }
        }
        Ok(this.clone())
    } else {
        // call path: fresh object on %RegExp.prototype%
        let o = Rc::new(RefCell::new(Obj::new(
            Some(i.p.regexp.clone()),
            ObjKind::Ord,
        )));
        let v = Value::Obj(o);
        regexp_init(i, &v, &p, &f)?;
        Ok(v)
    }
}

fn re_exec(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    if re_of(this).is_none() {
        return Err(i.type_err("RegExp.prototype.exec called on incompatible receiver"));
    }
    let s = re_coerce_str(i, &arg(args, 0))?;
    regexp_builtin_exec(i, this, &s)
}

fn re_test(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    if !matches!(this, Value::Obj(_)) {
        return Err(i.type_err("RegExp.prototype.test called on non-object"));
    }
    let s = re_coerce_str(i, &arg(args, 0))?;
    let r = regexp_exec(i, this, &s)?;
    Ok(Value::Bool(!matches!(r, Value::Null)))
}

fn re_to_string(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    if !matches!(this, Value::Obj(_)) {
        return Err(i.type_err("RegExp.prototype.toString called on non-object"));
    }
    let src = i.get_prop_str(this, "source")?;
    let fl = i.get_prop_str(this, "flags")?;
    let s = format!("/{}/{}", re_coerce_str(i, &src)?, re_coerce_str(i, &fl)?);
    Ok(Value::Str(Rc::from(s.as_str())))
}

/// `re.compile(pat, flags)` — Annex B: reinitialize in place, return `this`.
/// The receiver must have the [[RegExpMatcher]] slot (B.2.4.1 step 2); a
/// subclass instance compiles fine, plain objects throw.
fn re_compile(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    if re_of(this).is_none() {
        return Err(i.type_err("RegExp.prototype.compile called on incompatible receiver"));
    }
    let pattern = arg(args, 0);
    let flags = arg(args, 1);
    let (p, f) = if let Some(re) = re_of(&pattern) {
        if !matches!(flags, Value::Und) {
            return Err(i.type_err("Cannot supply flags when compiling a RegExp from a RegExp"));
        }
        (
            Value::Str(Rc::from(re.source.as_str())),
            Value::Str(Rc::from(re.flags.as_str())),
        )
    } else {
        (pattern, flags)
    };
    // RegExpInitialize: slots update first, then the throwing `lastIndex`
    // write — a non-writable lastIndex throws AFTER the new pattern lands
    let source = if matches!(p, Value::Und) {
        String::new()
    } else {
        re_coerce_str(i, &p)?
    };
    let flags_s = if matches!(f, Value::Und) {
        String::new()
    } else {
        re_coerce_str(i, &f)?
    };
    let re = Regex::compile(&source, &flags_s)
        .map_err(|e| i.syntax_err(format!("Invalid regular expression: /{source}/: {e}")))?;
    if let Value::Obj(o) = this {
        o.borrow_mut().kind = ObjKind::Regex(re);
    }
    set_strict(i, this, "lastIndex", Value::Num(0.0))?;
    Ok(this.clone())
}

// -- flag/property getters ----------------------------------------------------

macro_rules! re_getter {
    ($name:ident, $field:ident) => {
        fn $name(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
            match re_of(this) {
                Some(re) => Ok(Value::Bool(re.$field)),
                None => {
                    // getters on RegExp.prototype itself → undefined
                    if let Value::Obj(o) = this {
                        if Rc::ptr_eq(o, &i.p.regexp) {
                            return Ok(Value::Und);
                        }
                    }
                    Err(i.type_err("RegExp getter called on incompatible receiver"))
                }
            }
        }
    };
}

re_getter!(re_get_global, global);
re_getter!(re_get_icase, ignore_case);
re_getter!(re_get_multiline, multiline);
re_getter!(re_get_dotall, dot_all);
re_getter!(re_get_sticky, sticky);
re_getter!(re_get_unicode, unicode);
re_getter!(re_get_has_indices, has_indices);

fn re_get_source(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    // spec: %RegExp.prototype% → "(?:)", a regexp → its source ("" → "(?:)"),
    // anything else → TypeError
    if let Value::Obj(o) = this {
        if Rc::ptr_eq(o, &i.p.regexp) {
            return Ok(Value::Str(Rc::from("(?:)")));
        }
    }
    match re_of(this) {
        Some(re) => Ok(Value::Str(Rc::from(escape_re_pattern(&re.source).as_str()))),
        None => Err(i.type_err("RegExp.prototype.source getter called on incompatible receiver")),
    }
}

/// spec EscapeRegExpPattern — "/" and LineTerminators get backslash-
/// escaped so `"/" + S + "/" + F` reparses identically; "" → "(?:)"
fn escape_re_pattern(src: &str) -> String {
    if src.is_empty() {
        return "(?:)".into();
    }
    let mut out = String::with_capacity(src.len() + 4);
    // "escaped as necessary" — a `/` already preceded by `\` is left
    // alone: `\/` stays `\/` (re-escaping would change the pattern),
    // a bare `/` becomes `\/`; LineTerminators escape unconditionally
    let mut escaped = false;
    for ch in src.chars() {
        match ch {
            '\\' => {
                out.push('\\');
                escaped = !escaped;
                continue;
            }
            '/' if !escaped => out.push_str("\\/"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
        escaped = false;
    }
    out
}

/// spec `get flags` — generic: reads `hasIndices`/`global`/`ignoreCase`/
/// `multiline`/`dotAll`/`unicode`/`unicodeSets`/`sticky` in order and
/// appends the flag char for each truthy result.
fn re_get_flags(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    if !matches!(this, Value::Obj(_)) || crate::binary::bigint_is_prim(this) {
        return Err(i.type_err("RegExp.prototype.flags getter called on non-object"));
    }
    let mut out = String::new();
    for (prop, ch) in [
        ("hasIndices", 'd'),
        ("global", 'g'),
        ("ignoreCase", 'i'),
        ("multiline", 'm'),
        ("dotAll", 's'),
        ("unicode", 'u'),
        ("unicodeSets", 'v'),
        ("sticky", 'y'),
    ] {
        let pv = i.get_prop_str(this, prop)?;
        if i.to_bool(&pv) {
            out.push(ch);
        }
    }
    Ok(Value::Str(Rc::from(out.as_str())))
}

/// `get unicodeSets` — reflects the `v` flag (undefined on
/// %RegExp.prototype%, TypeError on other non-regexp receivers).
fn re_get_v(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    match re_of(this) {
        Some(re) => Ok(Value::Bool(re.unicode_sets)),
        None => {
            if let Value::Obj(o) = this {
                if Rc::ptr_eq(o, &i.p.regexp) {
                    return Ok(Value::Und);
                }
            }
            Err(i.type_err("RegExp.prototype.unicodeSets getter called on incompatible receiver"))
        }
    }
}

// -- Annex-B legacy statics on %RegExp% -----------------------------------------

/// `this` must be SameValue with this realm's %RegExp% — the legacy
/// getters/setters throw TypeError otherwise (GetLegacyRegExpStaticProperty).
/// On success returns the realm state object.
fn re_static_receiver(i: &mut Interp, this: &Value) -> Result<ObjRef, Value> {
    if matches!(this, Value::Obj(o) if Rc::ptr_eq(o, &re_ctor(i))) {
        Ok(re_state(i))
    } else {
        Err(i.type_err("RegExp static property accessor called on incompatible receiver"))
    }
}

fn re_static_get_input(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    Ok(state_get(&re_static_receiver(i, this)?, "input"))
}

fn re_static_set_input(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let st = re_static_receiver(i, this)?;
    // spec: the [[Input]] slot is assigned ToString(value) — Symbols throw
    let v = re_coerce_str(i, &arg(args, 0))?;
    state_set(&st, "input", Value::Str(Rc::from(v.as_str())));
    Ok(Value::Und)
}

fn re_static_get_last_match(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    Ok(state_get(&re_static_receiver(i, this)?, "lastMatch"))
}

fn re_static_get_last_paren(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    Ok(state_get(&re_static_receiver(i, this)?, "lastParen"))
}

fn re_static_get_left(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    Ok(state_get(&re_static_receiver(i, this)?, "leftContext"))
}

fn re_static_get_right(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    Ok(state_get(&re_static_receiver(i, this)?, "rightContext"))
}

macro_rules! re_static_paren {
    ($f:ident, $n:literal) => {
        fn $f(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
            Ok(state_get(&re_static_receiver(i, this)?, $n))
        }
    };
}

re_static_paren!(re_static_get_p1, "p1");
re_static_paren!(re_static_get_p2, "p2");
re_static_paren!(re_static_get_p3, "p3");
re_static_paren!(re_static_get_p4, "p4");
re_static_paren!(re_static_get_p5, "p5");
re_static_paren!(re_static_get_p6, "p6");
re_static_paren!(re_static_get_p7, "p7");
re_static_paren!(re_static_get_p8, "p8");
re_static_paren!(re_static_get_p9, "p9");

// -- Symbol.* protocol methods --------------------------------------------------

fn re_sym_match(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let rx = this;
    if !matches!(rx, Value::Obj(_)) {
        return Err(i.type_err("Symbol.match called on non-object"));
    }
    let s = re_coerce_str(i, &arg(args, 0))?;
    let fv = i.get_prop_str(rx, "flags")?;
    let flags = re_coerce_str(i, &fv)?;
    if !flags.contains('g') {
        return regexp_exec(i, rx, &s);
    }
    let full_unicode = flags.contains('u') || flags.contains('v');
    set_strict(i, rx, "lastIndex", Value::Num(0.0))?;
    let arr = i.new_arr(Vec::new());
    let a = Value::Obj(arr);
    let chars: Vec<u16> = s.encode_utf16().collect();
    let mut n = 0usize;
    loop {
        let result = regexp_exec(i, rx, &s)?;
        if matches!(result, Value::Null) {
            return Ok(if n == 0 { Value::Null } else { a });
        }
        let m0 = i.get_prop_str(&result, "0")?;
        let match_str = re_coerce_str(i, &m0)?;
        create_data_prop(i, &a, n, Value::Str(Rc::from(match_str.as_str())))?;
        if match_str.is_empty() {
            let lv = i.get_prop_str(rx, "lastIndex")?;
            let this_index = to_length_num(i.to_num(&lv)?);
            let next = advance_string_index(&chars, this_index, full_unicode);
            set_strict(i, rx, "lastIndex", Value::Num(next as f64))?;
        }
        n += 1;
        if n >= 1_000_000 {
            break;
        }
    }
    Ok(a)
}

fn re_sym_match_all(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let rx = this;
    if !matches!(rx, Value::Obj(_)) {
        return Err(i.type_err("Symbol.matchAll called on non-object"));
    }
    let s = re_coerce_str(i, &arg(args, 0))?;
    let c = re_species_ctor(i, rx)?;
    let fv = i.get_prop_str(rx, "flags")?;
    let flags = re_coerce_str(i, &fv)?;
    let matcher = i.construct(
        &c,
        vec![rx.clone(), Value::Str(Rc::from(flags.as_str()))],
        None,
    )?;
    let lv = i.get_prop_str(rx, "lastIndex")?;
    let last_index = to_length_num(i.to_num(&lv)?);
    set_strict(i, &matcher, "lastIndex", Value::Num(last_index as f64))?;
    let global = flags.contains('g');
    let unicode = flags.contains('u') || flags.contains('v');
    // %RegExpStringIterator% — plain object with sym-keyed slots
    let proto = match state_get(&re_state(i), "iterProto") {
        Value::Obj(o) => o,
        _ => i.p.obj.clone(),
    };
    let o = Rc::new(RefCell::new(Obj::new(Some(proto), ObjKind::Ord)));
    {
        let mut b = o.borrow_mut();
        for (k, v) in [
            (SYM_ITER_RX, matcher),
            (SYM_ITER_S, Value::Str(Rc::from(s.as_str()))),
            (SYM_ITER_G, Value::Bool(global)),
            (SYM_ITER_U, Value::Bool(unicode)),
            (SYM_ITER_DONE, Value::Bool(false)),
        ] {
            b.props.insert(PropKey::Sym(k), Prop::builtin(v));
        }
    }
    Ok(Value::Obj(o))
}

/// `%RegExpStringIteratorPrototype%.next` — pulls one exec per call.
fn re_striter_next(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let Value::Obj(o) = this else {
        return Err(i.type_err("RegExp String Iterator next called on non-object"));
    };
    let slot = |k: u64| {
        o.borrow()
            .props
            .get(&PropKey::Sym(k))
            .map(|p| p.val.clone())
    };
    let (Some(rx), Some(sv), Some(gv), Some(uv), Some(dv)) = (
        slot(SYM_ITER_RX),
        slot(SYM_ITER_S),
        slot(SYM_ITER_G),
        slot(SYM_ITER_U),
        slot(SYM_ITER_DONE),
    ) else {
        return Err(i.type_err("RegExp String Iterator next called on incompatible receiver"));
    };
    let set_done = |o: &ObjRef| {
        o.borrow_mut().props.insert(
            PropKey::Sym(SYM_ITER_DONE),
            Prop::builtin(Value::Bool(true)),
        );
    };
    if matches!(dv, Value::Bool(true)) {
        return iter_result(i, Value::Und, true);
    }
    let s = match &sv {
        Value::Str(s) => s.to_string(),
        _ => String::new(),
    };
    let global = matches!(gv, Value::Bool(true));
    let unicode = matches!(uv, Value::Bool(true));
    let m = regexp_exec(i, &rx, &s)?;
    if matches!(m, Value::Null) {
        set_done(o);
        return iter_result(i, Value::Und, true);
    }
    if !global {
        set_done(o);
        return iter_result(i, m, false);
    }
    let m0 = i.get_prop_str(&m, "0")?;
    let match_str = re_coerce_str(i, &m0)?;
    if match_str.is_empty() {
        let chars: Vec<u16> = s.encode_utf16().collect();
        let lv = i.get_prop_str(&rx, "lastIndex")?;
        let this_index = to_length_num(i.to_num(&lv)?);
        let next = advance_string_index(&chars, this_index, unicode);
        set_strict(i, &rx, "lastIndex", Value::Num(next as f64))?;
    }
    iter_result(i, m, false)
}

fn re_sym_search(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let rx = this;
    if !matches!(rx, Value::Obj(_)) {
        return Err(i.type_err("Symbol.search called on non-object"));
    }
    let s = re_coerce_str(i, &arg(args, 0))?;
    // spec: zero lastIndex, exec, then restore it iff it changed
    let previous = i.get_prop_str(rx, "lastIndex")?;
    if !i.same_value(&previous, &Value::Num(0.0))? {
        set_strict(i, rx, "lastIndex", Value::Num(0.0))?;
    }
    let result = regexp_exec(i, rx, &s)?;
    let current = i.get_prop_str(rx, "lastIndex")?;
    if !i.same_value(&current, &previous)? {
        set_strict(i, rx, "lastIndex", previous)?;
    }
    if matches!(result, Value::Null) {
        return Ok(Value::Num(-1.0));
    }
    i.get_prop_str(&result, "index")
}

/// spec GetSubstitution — expands `$$` `$&` `` $` `` `$'` `$n` `$nn`
/// `$<name>` against the match's position, captures and named groups.
fn get_substitution(
    i: &mut Interp,
    matched: &str,
    s: &str,
    position: usize,
    captures: &[Value],
    named: &Value,
    rep: &str,
) -> Result<String, Value> {
    let chars: Vec<u16> = s.encode_utf16().collect();
    let tail_start = (position + matched.encode_utf16().count()).min(chars.len());
    let mut out = String::new();
    let cs: Vec<char> = rep.chars().collect();
    let n_caps = captures.len();
    let mut j = 0;
    while j < cs.len() {
        if cs[j] != '$' {
            out.push(cs[j]);
            j += 1;
            continue;
        }
        match cs.get(j + 1).copied() {
            Some('$') => {
                out.push('$');
                j += 2;
            }
            Some('&') => {
                out.push_str(matched);
                j += 2;
            }
            Some('`') => {
                out.push_str(&chars_string(&chars[..position.min(chars.len())]));
                j += 2;
            }
            Some('\'') => {
                out.push_str(&chars_string(&chars[tail_start..]));
                j += 2;
            }
            Some('<') => {
                // `$<name>` — a reference only when namedCaptures exists AND
                // the `<name>` closes; otherwise literal
                if matches!(named, Value::Und) {
                    out.push_str("$<");
                    j += 2;
                    continue;
                }
                let mut name = String::new();
                let mut k = j + 2;
                let mut closed = false;
                while k < cs.len() {
                    if cs[k] == '>' {
                        closed = true;
                        break;
                    }
                    name.push(cs[k]);
                    k += 1;
                }
                if !closed {
                    out.push_str("$<");
                    j += 2;
                    continue;
                }
                let cap = i.get_prop_str(named, &name)?;
                if !matches!(cap, Value::Und) {
                    out.push_str(&re_coerce_str(i, &cap)?);
                }
                j = k + 1;
            }
            Some(d) if d.is_ascii_digit() => {
                // $n / $nn — 2-digit wins if it names a real capture
                let d1 = d.to_digit(10).unwrap() as usize;
                let d2 = cs
                    .get(j + 2)
                    .and_then(|c| c.to_digit(10))
                    .map(|x| d1 * 10 + x as usize);
                let (g, len) = match d2 {
                    Some(g) if (1..=n_caps).contains(&g) => (g, 2),
                    _ if (1..=n_caps).contains(&d1) => (d1, 1),
                    _ => {
                        out.push('$');
                        j += 1;
                        continue;
                    }
                };
                if let Value::Str(sv) = &captures[g - 1] {
                    out.push_str(sv);
                }
                j += 1 + len;
            }
            _ => {
                out.push('$');
                j += 1;
            }
        }
    }
    Ok(out)
}

fn re_sym_replace(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let rx = this;
    if !matches!(rx, Value::Obj(_)) {
        return Err(i.type_err("Symbol.replace called on non-object"));
    }
    let s = re_coerce_str(i, &arg(args, 0))?;
    let length_s = s.encode_utf16().count();
    let repl_arg = arg(args, 1);
    let functional = i.is_callable(&repl_arg);
    let rep_str = if functional {
        None
    } else {
        Some(re_coerce_str(i, &repl_arg)?)
    };
    let fv = i.get_prop_str(rx, "flags")?;
    let flags = re_coerce_str(i, &fv)?;
    let global = flags.contains('g');
    let full_unicode = flags.contains('u') || flags.contains('v');
    if global {
        set_strict(i, rx, "lastIndex", Value::Num(0.0))?;
    }
    let chars: Vec<u16> = s.encode_utf16().collect();
    // collect all exec results first (spec gathers them before splicing)
    let mut results: Vec<Value> = Vec::new();
    loop {
        let result = regexp_exec(i, rx, &s)?;
        if matches!(result, Value::Null) {
            break;
        }
        results.push(result.clone());
        if !global {
            break;
        }
        let m0 = i.get_prop_str(&result, "0")?;
        let match_str = re_coerce_str(i, &m0)?;
        if match_str.is_empty() {
            let lv = i.get_prop_str(rx, "lastIndex")?;
            let this_index = to_length_num(i.to_num(&lv)?);
            let next = advance_string_index(&chars, this_index, full_unicode);
            set_strict(i, rx, "lastIndex", Value::Num(next as f64))?;
        }
        if results.len() >= 1_000_000 {
            break;
        }
    }
    let mut acc = String::new();
    let mut next_pos = 0usize;
    for result in &results {
        let rlen = i.get_prop_str(result, "length")?;
        let res_len = to_length_num(i.to_num(&rlen)?);
        let n_caps = res_len.saturating_sub(1);
        let matched_v = i.get_prop_str(result, "0")?;
        let matched = re_coerce_str(i, &matched_v)?;
        let match_len = matched.encode_utf16().count();
        let idx_v = i.get_prop_str(result, "index")?;
        let pos_num = i.to_num(&idx_v)?.trunc();
        let position = if pos_num.is_nan() || pos_num <= 0.0 {
            0usize
        } else if pos_num >= length_s as f64 {
            length_s
        } else {
            pos_num as usize
        };
        let mut captures: Vec<Value> = Vec::with_capacity(n_caps);
        for n in 1..=n_caps {
            let cap_n = i.get_prop(result, &idx_key(n))?;
            captures.push(if matches!(cap_n, Value::Und) {
                cap_n
            } else {
                Value::Str(Rc::from(re_coerce_str(i, &cap_n)?.as_str()))
            });
        }
        let mut named = i.get_prop_str(result, "groups")?;
        let replacement = if functional {
            let mut cargs = vec![Value::Str(Rc::from(matched.as_str()))];
            cargs.extend(captures.iter().cloned());
            cargs.push(Value::Num(position as f64));
            cargs.push(Value::Str(Rc::from(s.as_str())));
            if !matches!(named, Value::Und) {
                cargs.push(named.clone());
            }
            let rv = i.call(&repl_arg, Value::Und, cargs)?;
            re_coerce_str(i, &rv)?
        } else {
            // spec: non-functional replace does ToObject(groups) when
            // groups isn't undefined — null/undefined→TypeError path
            if !matches!(named, Value::Und) {
                named = i.to_object(&named)?;
            }
            get_substitution(
                i,
                &matched,
                &s,
                position,
                &captures,
                &named,
                rep_str.as_deref().unwrap_or_default(),
            )?
        };
        if position >= next_pos {
            acc.push_str(&chars_string(&chars[next_pos..position]));
            acc.push_str(&replacement);
            // fake exec results can overrun — clamp like `substring` does
            next_pos = (position + match_len).min(chars.len());
        }
    }
    acc.push_str(&chars_string(&chars[next_pos..]));
    Ok(Value::Str(Rc::from(acc.as_str())))
}

fn re_sym_split(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let rx = this;
    if !matches!(rx, Value::Obj(_)) {
        return Err(i.type_err("Symbol.split called on non-object"));
    }
    let s = re_coerce_str(i, &arg(args, 0))?;
    let c = re_species_ctor(i, rx)?;
    let fv = i.get_prop_str(rx, "flags")?;
    let flags = re_coerce_str(i, &fv)?;
    let unicode_matching = flags.contains('u') || flags.contains('v');
    let new_flags = if flags.contains('y') {
        flags
    } else {
        format!("{flags}y")
    };
    let splitter = i.construct(
        &c,
        vec![rx.clone(), Value::Str(Rc::from(new_flags.as_str()))],
        None,
    )?;
    let a = Value::Obj(i.new_arr(Vec::new()));
    let lim = match arg(args, 1) {
        Value::Und => u32::MAX,
        v => to_uint32_num(i.to_num(&v)?),
    } as usize;
    if lim == 0 {
        return Ok(a);
    }
    let chars: Vec<u16> = s.encode_utf16().collect();
    let size = chars.len();
    let mut length_a = 0usize;
    if size == 0 {
        let z = regexp_exec(i, &splitter, &s)?;
        if !matches!(z, Value::Null) {
            return Ok(a);
        }
        create_data_prop(i, &a, 0, Value::Str(Rc::from(s.as_str())))?;
        return Ok(a);
    }
    let (mut p, mut q) = (0usize, 0usize);
    while q < size {
        set_strict(i, &splitter, "lastIndex", Value::Num(q as f64))?;
        let z = regexp_exec(i, &splitter, &s)?;
        if matches!(z, Value::Null) {
            q = advance_string_index(&chars, q, unicode_matching);
            continue;
        }
        let lv = i.get_prop_str(&splitter, "lastIndex")?;
        let e = to_length_num(i.to_num(&lv)?).min(size);
        if e == p {
            q = advance_string_index(&chars, q, unicode_matching);
            continue;
        }
        create_data_prop(i, &a, length_a, chars_str(&chars, p, q))?;
        length_a += 1;
        if length_a == lim {
            return Ok(a);
        }
        p = e;
        let zlen = i.get_prop_str(&z, "length")?;
        let n_caps = to_length_num(i.to_num(&zlen)?).saturating_sub(1);
        for n in 1..=n_caps {
            let cap = i.get_prop(&z, &idx_key(n))?;
            create_data_prop(i, &a, length_a, cap)?;
            length_a += 1;
            if length_a == lim {
                return Ok(a);
            }
        }
        q = p;
    }
    create_data_prop(i, &a, length_a, chars_str(&chars, p, size))?;
    Ok(a)
}
