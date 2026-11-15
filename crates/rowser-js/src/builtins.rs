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
fn own_prop_enumerable(
    i: &mut Interp,
    v: &Value,
    k: &PropKey,
) -> Result<bool, Value> {
    let Value::Obj(o) = v else {
        return Ok(false);
    };
    if let ObjKind::Proxy { target, handler } = &o.borrow().kind {
        let (t, h) = (target.clone(), handler.clone());
        return match i.proxy_trap(&h, "getOwnPropertyDescriptor")? {
            Some(trap) => {
                let kv = i.key_to_value(k);
                let r = i.call(&trap, h, vec![t, kv])?;
                match r {
                    Value::Und => Ok(false),
                    Value::Obj(_) => {
                        let e = i.get_prop_str(&r, "enumerable")?;
                        Ok(i.to_bool(&e))
                    }
                    _ => Err(i.type_err(
                        "proxy gOPD trap must return object or undefined",
                    )),
                }
            }
            None => own_prop_enumerable(i, &t, k),
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
                    n < items.len()
                        && !b.holes.as_ref().is_some_and(|h| h.contains(&n))
                }
                ObjKind::Args { items, .. } => {
                    n < items.len()
                        && !b.holes.as_ref().is_some_and(|h| h.contains(&n))
                }
                ObjKind::Typed { buf, off, len, kind, track } => {
                    typed_state(buf, *off, *len, *kind, *track)
                        .is_some_and(|eff| n < eff)
                }
                _ => false,
            });
        }
    }
    Ok(b.props.get(k).map(|p| p.enumerable).unwrap_or(false))
}

fn own_enum_entries(
    i: &Interp,
    v: &Value,
) -> Vec<(PropKey, Option<Value>)> {
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
        ObjKind::Typed { buf, off, len, kind, track } => {
            for k in 0..typed_state(buf, *off, *len, *kind, *track).unwrap_or(0)
            {
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
        } else if let (PropKey::S(s), ObjKind::Args { map, .. }) =
            (k, &b.kind)
        {
            // a still-mapped arguments index reads the env binding, not
            // the stored prop value
            if arr_index(s)
                .is_some_and(|n| matches!(map.get(n), Some(Some(_))))
            {
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
    let mut out: Vec<(PropKey, Option<Value>)> = ints
        .into_iter()
        .map(|(n, v)| (idx_key(n), v))
        .collect();
    out.extend(strs);
    out.extend(syms);
    out
}

/// EnumerableOwnProperties — like `own_enum_entries` but resolves every
/// value and routes proxies through their ownKeys/gOPD traps.
fn own_enum_entries_full(
    i: &mut Interp,
    v: &Value,
) -> Result<Vec<(PropKey, Value)>, Value> {
    if let Value::Obj(o) = v {
        if matches!(&o.borrow().kind, ObjKind::Proxy { .. }) {
            let mut out = Vec::new();
            for k in own_keys_all(i, v)? {
                let kv = i.key_to_value(&k);
                let d =
                    obj_get_own_prop_desc(i, &Value::Und, &[v.clone(), kv])?;
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
    let mut out = Vec::new();
    for (k, slot) in own_enum_entries(i, v) {
        let val = match slot {
            Some(val) => val,
            None => i.get_prop(v, &k)?,
        };
        out.push((k, val));
    }
    Ok(out)
}

/// `[[OwnPropertyKeys]]` — all own keys incl non-enumerable, in spec order:
/// integer-ascending, then strings insertion-order, then symbols.
fn own_keys_all(i: &mut Interp, v: &Value) -> Result<Vec<PropKey>, Value> {
    let Value::Obj(o) = v else {
        return Ok(Vec::new());
    };
    if let ObjKind::Proxy { target, handler } = &o.borrow().kind {
        let (t, h) = (target.clone(), handler.clone());
        return match i.proxy_trap(&h, "ownKeys")? {
            Some(trap) => {
                let r = i.call(&trap, h, vec![t])?;
                let mut out = Vec::new();
                for k in i.iter_values(&r)? {
                    match k {
                        Value::Str(s) => out.push(PropKey::S(s)),
                        Value::Sym(id, _) => out.push(PropKey::Sym(id)),
                        _ => {
                            return Err(i.type_err(
                                "proxy ownKeys must return strings or symbols",
                            ))
                        }
                    }
                }
                Ok(out)
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
        ObjKind::Typed { buf, off, len, kind, track } => {
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
    ("at", 1.0), ("concat", 1.0), ("copyWithin", 2.0), ("every", 1.0),
    ("fill", 1.0), ("filter", 1.0), ("find", 1.0), ("findIndex", 1.0),
    ("findLast", 1.0), ("findLastIndex", 1.0), ("flatMap", 1.0),
    ("forEach", 1.0), ("includes", 1.0), ("indexOf", 1.0), ("join", 1.0),
    ("lastIndexOf", 1.0), ("map", 1.0), ("push", 1.0), ("reduce", 1.0),
    ("reduceRight", 1.0), ("slice", 2.0), ("some", 1.0), ("sort", 1.0),
    ("splice", 2.0), ("unshift", 1.0), ("with", 2.0), ("toSorted", 1.0),
    ("toSpliced", 2.0), ("from", 1.0), ("isArray", 1.0),
    // Iterator helpers (unique names)
    ("chunks", 1.0), ("windows", 1.0),
    // Object
    ("assign", 2.0), ("create", 2.0), ("defineProperties", 2.0),
    ("defineProperty", 3.0), ("entries", 1.0), ("freeze", 1.0),
    ("fromEntries", 1.0), ("getOwnPropertyDescriptor", 2.0),
    ("getOwnPropertyDescriptors", 1.0), ("getOwnPropertyNames", 1.0),
    ("getOwnPropertySymbols", 1.0), ("getPrototypeOf", 1.0),
    ("groupBy", 2.0), ("hasOwn", 2.0), ("is", 2.0), ("isExtensible", 1.0),
    ("isFrozen", 1.0), ("isSealed", 1.0), ("keys", 1.0),
    ("preventExtensions", 1.0), ("seal", 1.0), ("setPrototypeOf", 2.0),
    ("values", 1.0), ("hasOwnProperty", 1.0), ("isPrototypeOf", 1.0),
    ("propertyIsEnumerable", 1.0),
    // Function
    ("apply", 2.0), ("bind", 1.0), ("call", 1.0),
    // String
    ("charAt", 1.0), ("charCodeAt", 1.0), ("codePointAt", 1.0),
    ("endsWith", 1.0), ("localeCompare", 1.0), ("match", 1.0),
    ("matchAll", 1.0), ("padEnd", 1.0), ("padStart", 1.0), ("repeat", 1.0),
    ("replace", 2.0), ("replaceAll", 2.0), ("search", 1.0),
    ("split", 2.0), ("startsWith", 1.0), ("substring", 2.0),
    ("fromCharCode", 0.0), ("fromCodePoint", 0.0), ("raw", 1.0),
    // Number
    ("isFinite", 1.0), ("isInteger", 1.0), ("isNaN", 1.0),
    ("isSafeInteger", 1.0), ("parseFloat", 1.0), ("parseInt", 2.0),
    ("toExponential", 1.0), ("toFixed", 1.0), ("toPrecision", 1.0),
    // Math
    ("abs", 1.0), ("acos", 1.0), ("acosh", 1.0), ("asin", 1.0),
    ("asinh", 1.0), ("atan", 1.0), ("atan2", 2.0), ("atanh", 1.0),
    ("cbrt", 1.0), ("ceil", 1.0), ("clz32", 1.0), ("cos", 1.0),
    ("cosh", 1.0), ("exp", 1.0), ("expm1", 1.0), ("floor", 1.0),
    ("fround", 1.0), ("imul", 2.0), ("log", 1.0), ("log10", 1.0),
    ("log1p", 1.0), ("log2", 1.0), ("pow", 2.0), ("round", 1.0),
    ("sign", 1.0), ("sin", 1.0), ("sinh", 1.0), ("sqrt", 1.0),
    ("tan", 1.0), ("tanh", 1.0), ("trunc", 1.0),
    // JSON
    ("parse", 2.0), ("stringify", 3.0),
    // Reflect
    ("construct", 2.0), ("deleteProperty", 2.0), ("ownKeys", 1.0),
    // Promise
    ("all", 1.0), ("allSettled", 1.0), ("any", 1.0), ("race", 1.0),
    ("reject", 1.0), ("resolve", 1.0), ("then", 2.0), ("catch", 1.0),
    ("finally", 1.0),
    // Map/Set (name collisions with Reflect resolved to the Map/Set
    // arities — same tests, better coverage)
    ("add", 1.0), ("delete", 1.0), ("get", 1.0), ("has", 1.0),
    ("set", 2.0),
    // RegExp / Symbol / globals / misc
    ("exec", 1.0), ("test", 1.0), ("compile", 2.0), ("for", 1.0),
    ("[Symbol.match]", 1.0), ("[Symbol.matchAll]", 1.0),
    ("[Symbol.replace]", 2.0), ("[Symbol.search]", 1.0),
    ("[Symbol.split]", 2.0),
    ("keyFor", 1.0), ("eval", 1.0), ("decodeURI", 1.0),
    ("decodeURIComponent", 1.0), ("encodeURI", 1.0),
    ("encodeURIComponent", 1.0), ("escape", 1.0), ("unescape", 1.0),
    // Iterator helpers
    ("take", 1.0), ("drop", 1.0), ("next", 0.0), ("return", 0.0),
    // Annex-B string HTML + misc
    ("anchor", 1.0), ("link", 1.0), ("fontcolor", 1.0), ("fontsize", 1.0),
    ("big", 0.0), ("blink", 0.0), ("bold", 0.0), ("fixed", 0.0),
    ("italics", 0.0), ("small", 0.0), ("strike", 0.0), ("sub", 0.0),
    ("sup", 0.0), ("substr", 2.0), ("trimStart", 0.0), ("trimEnd", 0.0),
    ("trimLeft", 0.0), ("trimRight", 0.0),
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

fn arr_clone(this: &Value) -> Vec<Value> {
    if let Value::Obj(o) = this {
        if let ObjKind::Arr(items) = &o.borrow().kind {
            return items.clone();
        }
    }
    Vec::new()
}

fn with_arr(this: &Value, f: impl FnOnce(&mut Vec<Value>)) {
    if let Value::Obj(o) = this {
        if let ObjKind::Arr(items) = &mut o.borrow_mut().kind {
            f(items);
        }
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
                        ObjKind::Arr(items) if len == items.len()
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
fn elems_len(i: &mut Interp, v: &Value) -> Result<usize, Value> {
    if let Value::Obj(o) = v {
        if matches!(&o.borrow().kind, ObjKind::Arr(_)) {
            return Ok(arr_declared_len(o));
        }
        if let ObjKind::Iter { items, .. } = &o.borrow().kind {
            return Ok(items.len());
        }
    }
    let len = i.get_prop_str(v, "length")?;
    Ok((i.to_num(&len)?.max(0.0) as usize).min(8_000_000))
}

/// `(index, value)` for PRESENT elements only — spec HasProperty semantics.
/// Vec-backed arrays can't distinguish `[1,,3]` holes from explicit
/// `undefined`, so they yield every slot; generic receivers are checked
/// via `has_prop` per index.
pub(crate) fn to_elems_sparse(
    i: &mut Interp,
    v: &Value,
) -> Result<Vec<(usize, Value)>, Value> {
    match v {
        Value::Obj(o) => {
            // a fully dense array enumerates directly; holes, sparse
            // props entries and materialized slots need HasProperty
            // filtering (spec ArrayMethods skip absent indices)
            let dense = {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Arr(items) => {
                        let no_holes = b
                            .holes
                            .as_ref()
                            .map_or(true, |h| h.is_empty());
                        let declared = arr_declared_len(o);
                        if no_holes && declared == items.len() {
                            Some(items.iter().cloned().enumerate().collect())
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            };
            if let Some(v) = dense {
                return Ok(v);
            }
            if let ObjKind::Iter { items, .. } = &o.borrow().kind {
                return Ok(items.iter().cloned().enumerate().collect());
            }
            if matches!(o.borrow().kind, ObjKind::IterLazy(_)) {
                let items = iter_drain_lazy(i, v)?;
                return Ok(items.into_iter().enumerate().collect());
            }
        }
        Value::Und | Value::Null => {
            return Err(i.type_err("cannot convert undefined or null to object"));
        }
        _ => {}
    }
    let len = i.get_prop_str(v, "length")?;
    let n = (i.to_num(&len)?.max(0.0) as usize).min(8_000_000);
    let mut out = Vec::new();
    for k in 0..n {
        let key = idx_key(k);
        if i.has_prop(v, &key)? {
            out.push((k, i.get_prop(v, &key)?));
        }
    }
    Ok(out)
}

/// spec ArraySpeciesCreate: if `o` is an Array, result ctor =
/// `o.constructor[@@species]` (may be null→default); else default Array.
fn species_create(i: &mut Interp, o: &Value, len: usize) -> Result<Value, Value> {
    let mut c = Value::Und;
    if let Value::Obj(ob) = o {
        if matches!(ob.borrow().kind, ObjKind::Arr(_)) {
            c = i.get_prop_str(o, "constructor")?;
            if matches!(c, Value::Obj(_)) {
                c = i.get_prop(&c, &PropKey::Sym(SYM_SPECIES))?;
                if matches!(c, Value::Null) {
                    c = Value::Und;
                }
            }
        }
    }
    match c {
        Value::Und => Ok(Value::Obj(i.new_arr(vec![Value::Und; len]))),
        c if i.is_constructor(&c) => {
            i.construct(&c, vec![Value::Num(len as f64)], None)
        }
        _ => Err(i.type_err("invalid species constructor")),
    }
}

/// spec CreateDataPropertyOrThrow on integer index — the write semantics
/// array-into-species methods use (throws on non-configurable existing
/// props / non-extensible targets).
fn create_data_prop(
    i: &mut Interp,
    a: &Value,
    k: usize,
    v: Value,
) -> Result<(), Value> {
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
                let slot_exists = k < items.len()
                    && !b.holes.as_ref().is_some_and(|h| h.contains(&k));
                if !slot_exists && !b.extensible {
                    return Err(i.type_err("object is not extensible"));
                }
            }
            if k >= items.len() {
                let lkey = PropKey::from_str("length");
                let len_w =
                    b.props.get(&lkey).map(|p| p.writable).unwrap_or(true);
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
            Some(p) if !p.configurable => {
                return Err(i.type_err("Cannot redefine property"))
            }
            None if !b.extensible => {
                return Err(i.type_err("object is not extensible"))
            }
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
        method(i, &p, "toString", obj_to_string);
        method(i, &p, "toLocaleString", obj_to_locale_string);
        method(i, &p, "valueOf", obj_value_of);
        method(i, &p, "__defineGetter__", obj_define_getter);
        method(i, &p, "__defineSetter__", obj_define_setter);
        method(i, &p, "__lookupGetter__", obj_lookup_getter);
        method(i, &p, "__lookupSetter__", obj_lookup_setter);
        // __proto__ accessor
        let get = native(i, "get __proto__", obj_get_proto);
        let set = native(i, "set __proto__", obj_set_proto);
        p.borrow_mut().props.insert(
            pk("__proto__"),
            Prop::accessor(Some(get), Some(set)),
        );
    }

    // -- Function.prototype --
    {
        let p = i.p.func.clone();
        method(i, &p, "call", fn_call);
        method(i, &p, "apply", fn_apply);
        method(i, &p, "bind", fn_bind);
        method(i, &p, "toString", fn_to_string);
        // restricted function properties — the shared %ThrowTypeError%
        // accessor makes `fn.caller`/`fn.arguments` throw on get+set
        fn throw_type_err(i: &mut Interp, _t: &Value, _a: &[Value]) -> NResult {
            Err(i.type_err("'caller' and 'arguments' are restricted"))
        }
        for k in ["caller", "arguments"] {
            let g = native(i, "ThrowTypeError", throw_type_err);
            let s = native(i, "ThrowTypeError", throw_type_err);
            p.borrow_mut().props.insert(
                PropKey::from_str(k),
                Prop::accessor(Some(g), Some(s)),
            );
        }
        // Function.prototype[@@hasInstance] — {w:false, e:false, c:false}
        let hi = native(i, "[Symbol.hasInstance]", fn_has_instance);
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
        // @@unscopables — `with (arr)` must not resolve array method names
        let un = i.new_obj();
        for n in [
            "at", "concat", "copyWithin", "entries", "fill", "find",
            "findIndex", "findLast", "findLastIndex", "flat", "flatMap",
            "includes", "keys", "lastIndexOf", "pop", "push", "reduce",
            "reduceRight", "shift", "slice", "some", "splice", "values",
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
        method(i, &p, "reverse", arr_reverse);
        method(i, &p, "sort", arr_sort);
        method(i, &p, "at", arr_at);
        method(i, &p, "keys", arr_keys);
        method(i, &p, "values", arr_values);
        method(i, &p, "entries", arr_entries);
        method(i, &p, "toString", arr_join);
        method(i, &p, "toLocaleString", arr_join);
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
        p.borrow_mut().props.insert(PropKey::from_str("trimLeft"), Prop::builtin(ts));
        p.borrow_mut().props.insert(PropKey::from_str("trimRight"), Prop::builtin(te));
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
        method(i, &p, "toString", str_to_string);
        method(i, &p, "valueOf", str_to_string);
        method(i, &p, "toLocaleString", str_to_string);
        method(i, &p, "localeCompare", str_locale_compare);
        method(i, &p, "normalize", str_to_string);
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
        method(i, &p, "toString", bool_to_string);
        method(i, &p, "valueOf", bool_value_of);
    }

    // -- Symbol.prototype --
    {
        let p = i.p.symbol.clone();
        method(i, &p, "toString", sym_to_string);
        method(i, &p, "valueOf", sym_value_of);
        // Symbol.prototype[@@toPrimitive] — returns the wrapped symbol
        let stp = native(i, "[Symbol.toPrimitive]", sym_to_primitive);
        p.borrow_mut()
            .props
            .insert(PropKey::Sym(SYM_TOPRIMITIVE), Prop::builtin(stp));
        let get = native(i, "get description", sym_get_description);
        p.borrow_mut().props.insert(
            pk("description"),
            Prop::accessor(Some(get), None),
        );
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
        }
        // %GeneratorPrototype% — eager generators step a snapshot, so
        // `next` is the Iter stepper; `return`/`throw` finish the snapshot
        let gp = i.p.generator.clone();
        method(i, &gp, "next", iter_next);
        method(i, &gp, "return", gen_return);
        method(i, &gp, "throw", gen_throw);
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
        method(i, &p, "toString", re_to_string);
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
        // @@toStringTag → "RegExp"
        p.borrow_mut().props.insert(
            PropKey::Sym(SYM_TOSTRINGTAG),
            Prop {
                val: Value::Str(Rc::from("RegExp")),
                writable: false,
                enumerable: false,
                configurable: true,
                get: None,
                set: None,
            },
        );
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
    setter_ignores_proto(
        i,
        &home,
        this,
        &PropKey::Sym(SYM_TOSTRINGTAG),
        arg(args, 0),
    )
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
            let stack = corosensei::stack::DefaultStack::new(8 * 1024 * 1024)
                .expect("generator stack");
            corosensei::Coroutine::with_stack(stack,
                move |y: &corosensei::Yielder<GenIn, Value>, first: GenIn|
                      -> GenRet {
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
                    let v = i
                        .gen_ret_override
                        .take()
                        .or(ret_v)
                        .unwrap_or(Value::Und);
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
                            _ if mode == 2 => Value::Obj(i.new_arr(vec![
                                Value::Num(idx as f64),
                                item,
                            ])),
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
        _ => {
            match i.get_prop(v, &PropKey::Sym(SYM_ITERATOR))? {
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
        Value::Obj(o)
            if !crate::binary::bigint_is_prim(&Value::Obj(o.clone())) => {}
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
    if let IterOp::FlatMap { inner: Some(in_lz), .. } = &mut lz.op {
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

fn iter_zip_keyed_abrupt(
    i: &mut Interp,
    cols: &mut [(PropKey, IterLazy)],
    done: &mut [bool],
) {
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
                                    let p =
                                        padding.get(idx).cloned().unwrap_or(Value::Und);
                                    let k = cols[idx].0.clone();
                                    i.set_prop(&Value::Obj(o.clone()), k, p)?;
                                }
                                ZipMode::Strict => {
                                    if seen_live {
                                        let _ =
                                            iter_zip_keyed_close_srcs(i, cols, done);
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
        if let IterOp::FlatMap { inner: Some(in_lz), .. } = &mut lz.op {
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
                                return Err(i.type_err(
                                    "@@iterator result is not an object",
                                ));
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
            IterOp::Windows {
                size,
                partial,
                buf,
            } => {
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
    Ok(make_lazy(i, IterLazy {
        src: IterSrc::Empty,
        op: IterOp::Cat { rest },
        started: false,
    }))
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
fn iter_zip_padding(
    i: &mut Interp,
    padding: &Value,
    count: usize,
) -> Result<Vec<Value>, Value> {
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
    Ok(make_lazy(i, IterLazy {
        src: IterSrc::Empty,
        op: IterOp::Zip { srcs, done: vec![false; n], mode, padding },
        started: false,
    }))
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
    Ok(make_lazy(i, IterLazy {
        src: IterSrc::Empty,
        op: IterOp::ZipKeyed { cols, done: vec![false; n], mode, padding },
        started: false,
    }))
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
        lz.op = IterOp::Drop { left: to_skip.min(usize::MAX as f64) as usize };
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
    lz.op = IterOp::FlatMap { f, n: 0.0, inner: None };
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
        let species_get =
            native(i, "get [Symbol.species]", |_i, t, _a| Ok(t.clone()));
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
            native(i, "fromCodePoint", string_from_char_code),
        );
        i.set_builtin(o, "raw", native(i, "raw", string_raw));
        define_global(i, "String", ctor);
    }

    // -- Number --
    {
        let ctor = make_ctor(i, "Number", number_ctor, i.p.number.clone());
        let Value::Obj(o) = &ctor else { unreachable!() };
        for (n, f) in [
            ("parseInt", g_parse_int as NativeFn),
            ("parseFloat", g_parse_float),
            ("isNaN", number_is_nan),
            ("isFinite", number_is_finite),
            ("isInteger", number_is_integer),
            ("isSafeInteger", number_is_integer),
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
        let ctor = make_ctor(i, "Boolean", boolean_ctor, i.p.boolean.clone());
        define_global(i, "Boolean", ctor);
    }

    // -- Function --
    {
        let ctor = make_ctor(i, "Function", function_ctor, i.p.func.clone());
        define_global(i, "Function", ctor);
    }

    // -- Symbol --
    {
        let ctor = make_ctor(i, "Symbol", symbol_ctor, i.p.symbol.clone());
        let Value::Obj(o) = &ctor else { unreachable!() };
        i.set_builtin(
            o,
            "iterator",
            Value::Sym(SYM_ITERATOR, Rc::from("Symbol.iterator")),
        );
        i.set_builtin(
            o,
            "toPrimitive",
            Value::Sym(SYM_TOPRIMITIVE, Rc::from("Symbol.toPrimitive")),
        );
        i.set_builtin(
            o,
            "toStringTag",
            Value::Sym(SYM_TOSTRINGTAG, Rc::from("Symbol.toStringTag")),
        );
        i.set_builtin(
            o,
            "isConcatSpreadable",
            Value::Sym(SYM_IS_CONCAT_SPREADABLE, Rc::from("Symbol.isConcatSpreadable")),
        );
        i.set_builtin(
            o,
            "species",
            Value::Sym(SYM_SPECIES, Rc::from("Symbol.species")),
        );
        i.set_builtin(o, "for", native(i, "for", symbol_for));
        i.set_builtin(o, "keyFor", native(i, "keyFor", symbol_key_for));
        i.set_builtin(
            o,
            "hasInstance",
            Value::Sym(SYM_HASINSTANCE, Rc::from("Symbol.hasInstance")),
        );
        i.set_builtin(o, "match", Value::Sym(SYM_MATCH, Rc::from("Symbol.match")));
        i.set_builtin(
            o,
            "matchAll",
            Value::Sym(SYM_MATCHALL, Rc::from("Symbol.matchAll")),
        );
        i.set_builtin(
            o,
            "replace",
            Value::Sym(SYM_REPLACE, Rc::from("Symbol.replace")),
        );
        i.set_builtin(
            o,
            "search",
            Value::Sym(SYM_SEARCH, Rc::from("Symbol.search")),
        );
        i.set_builtin(o, "split", Value::Sym(SYM_SPLIT, Rc::from("Symbol.split")));
        i.set_builtin(
            o,
            "unscopables",
            Value::Sym(SYM_UNSCOPABLES, Rc::from("Symbol.unscopables")),
        );
        define_global(i, "Symbol", ctor);
    }

    // -- BigInt -- `BigInt(x)`/`123n` yield %BigInt.prototype% stand-ins
    // carrying an exact binary magnitude in `__bigint_x`
    {
        fn to_bigint(i: &mut Interp, v: &Value) -> Result<(bool, String), Value> {
            match v {
                Value::Num(n) => {
                    if !n.is_finite() || n.fract() != 0.0 {
                        return Err(i.range_err(format!(
                            "Cannot convert {n} to a BigInt"
                        )));
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
                    to_bigint(i, &p)
                }
            }
        }
        fn bigint_ctor(i: &mut Interp, _t: &Value, a: &[Value]) -> NResult {
            let (neg, mag) = to_bigint(i, &arg(a, 0))?;
            Ok(i.bigint_obj_b(neg, &mag))
        }
        fn bigint_as_int_n(i: &mut Interp, _t: &Value, a: &[Value]) -> NResult {
            let bits_n = i.to_num(&arg(a, 0))?;
            if !bits_n.is_finite() || bits_n < 0.0 || bits_n.fract() != 0.0 {
                return Err(i.range_err("Invalid BigInt64Array size"));
            }
            let (neg, mag) = to_bigint(i, &arg(a, 1))?;
            let (sn, sm) = crate::binary::bigint_as_int(
                (neg, &mag), bits_n as usize, true);
            Ok(i.bigint_obj_b(sn, &sm))
        }
        fn bigint_as_uint_n(i: &mut Interp, _t: &Value, a: &[Value]) -> NResult {
            let bits_n = i.to_num(&arg(a, 0))?;
            if !bits_n.is_finite() || bits_n < 0.0 || bits_n.fract() != 0.0 {
                return Err(i.range_err("Invalid BigInt64Array size"));
            }
            let (neg, mag) = to_bigint(i, &arg(a, 1))?;
            let (sn, sm) = crate::binary::bigint_as_int(
                (neg, &mag), bits_n as usize, false);
            Ok(i.bigint_obj_b(sn, &sm))
        }
        let ctor = make_ctor(i, "BigInt", bigint_ctor, i.p.bigint.clone());
        if let Value::Obj(co) = &ctor {
            i.set_builtin(&co.clone(), "asIntN",
                i.native("asIntN", bigint_as_int_n));
            i.set_builtin(&co.clone(), "asUintN",
                i.native("asUintN", bigint_as_uint_n));
        }
        define_global(i, "BigInt", ctor);
    }

    // -- Proxy -- `new Proxy(target, handler)`; MOP ops consult the
    // handler's traps and fall back to the target
    {
        fn proxy_ctor(i: &mut Interp, _t: &Value, a: &[Value]) -> NResult {
            if i.new_target_native.is_none() {
                return Err(i.type_err(
                    "Constructor Proxy requires 'new'",
                ));
            }
            let (t, h) = (arg(a, 0), arg(a, 1));
            if !matches!(t, Value::Obj(_)) || !matches!(h, Value::Obj(_)) {
                return Err(i.type_err(
                    "Cannot create proxy with a non-object as target or handler",
                ));
            }
            Ok(Value::Obj(Rc::new(RefCell::new(Obj::new(
                None,
                ObjKind::Proxy {
                    target: t,
                    handler: h,
                },
            )))))
        }
        // spec: Proxy has NO prototype property — and setting
        // Function.prototype.constructor here would clobber Function's
        // own backref (make_ctor writes `constructor` onto the proto obj)
        let o = Rc::new(RefCell::new(Obj::new(
            Some(i.p.func.clone()),
            ObjKind::Func(Func::Native {
                name: Rc::from("Proxy"),
                f: proxy_ctor,
            }),
        )));
        i.set_attr(&o, "name", Value::Str(Rc::from("Proxy")), false, true);
        i.set_attr(&o, "length", Value::Num(2.0), false, true);
        // IsConstructor marker for `new Proxy()` — a throwaway object
        let pmark = i.new_obj();
        i.set_attr(&o, "prototype", Value::Obj(pmark), false, false);
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
        let species_get =
            native(i, "get [Symbol.species]", |_i, t, _a| Ok(t.clone()));
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
        i.p.regexp.borrow_mut().props.insert(
            PropKey::Sym(SYM_RE_STATE),
            Prop::builtin(Value::Obj(state)),
        );
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
            ("exp", math_exp),
            ("sin", math_sin),
            ("cos", math_cos),
            ("tan", math_tan),
            ("asin", math_asin),
            ("acos", math_acos),
            ("atan", math_atan),
            ("atan2", math_atan2),
            ("hypot", math_hypot),
            ("imul", math_imul),
            ("clz32", math_clz32),
            ("fround", math_fround),
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
        define_global(i, "Math", Value::Obj(m));
    }

    // -- JSON --
    {
        let j = i.new_obj();
        method(i, &j, "parse", json_parse);
        method(i, &j, "stringify", json_stringify);
        define_global(i, "JSON", Value::Obj(j));
    }

    // -- errors --
    {
        let err = make_ctor(i, "Error", error_ctor, i.p.error.clone());
        define_global(i, "Error", err);
        let te = make_ctor(i, "TypeError", type_error_ctor, i.p.type_error.clone());
        define_global(i, "TypeError", te);
        let re = make_ctor(i, "RangeError", range_error_ctor, i.p.range_error.clone());
        define_global(i, "RangeError", re);
        let se = make_ctor(
            i,
            "SyntaxError",
            syntax_error_ctor,
            i.p.syntax_error.clone(),
        );
        define_global(i, "SyntaxError", se);
        let fe = make_ctor(i, "ReferenceError", ref_error_ctor, i.p.ref_error.clone());
        define_global(i, "ReferenceError", fe);
        let ee = make_ctor(i, "EvalError", eval_error_ctor, i.p.eval_error.clone());
        define_global(i, "EvalError", ee);
        let ue = make_ctor(i, "URIError", uri_error_ctor, i.p.uri_error.clone());
        define_global(i, "URIError", ue);
        let ae = make_ctor(i, "AggregateError", agg_error_ctor, i.p.agg_error.clone());
        define_global(i, "AggregateError", ae);
        let su = make_ctor(
            i,
            "SuppressedError",
            suppressed_error_ctor,
            i.p.suppressed_error.clone(),
        );
        define_global(i, "SuppressedError", su);
    }

    // -- Promise (stub: callbacks run synchronously) --
    {
        let p = i.p.promise.clone();
        method(i, &p, "then", promise_then);
        method(i, &p, "catch", promise_catch);
        method(i, &p, "finally", promise_finally);
        let ctor = make_ctor(i, "Promise", promise_ctor, p);
        let Value::Obj(o) = &ctor else { unreachable!() };
        i.set_builtin(o, "resolve", native(i, "resolve", promise_resolve_static));
        i.set_builtin(o, "reject", native(i, "reject", promise_reject_static));
        i.set_builtin(o, "all", native(i, "all", promise_all));
        i.set_builtin(o, "race", native(i, "race", promise_race));
        i.set_builtin(o, "allSettled", native(i, "allSettled", promise_all));
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
        method(i, &dp, "setDate", |i, t, a| {
            date_set(i, t, a, &[2], false)
        });
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
        dp.borrow_mut()
            .props
            .insert(PropKey::Sym(SYM_TOPRIMITIVE), Prop::builtin(stp));
        // Date.prototype[@@toStringTag] === "Date" — n.w, n.e, configurable
        dp.borrow_mut().props.insert(
            PropKey::Sym(SYM_TOSTRINGTAG),
            Prop {
                val: Value::Str(Rc::from("Date")),
                writable: false,
                enumerable: false,
                configurable: true,
                get: None,
                set: None,
            },
        );
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
        method(i, &mp, "keys", map_keys);
        method(i, &mp, "values", map_values);
        method(i, &mp, "entries", map_entries);
        let ent = i.obj_get_raw_str(&mp, "entries").unwrap();
        mp.borrow_mut()
            .props
            .insert(PropKey::Sym(SYM_ITERATOR), Prop::builtin(ent));
        let size_g = native(i, "get size", map_size);
        mp.borrow_mut().props.insert(
            pk("size"),
            Prop::accessor(Some(size_g), None),
        );
        let ctor = make_ctor(i, "Map", map_ctor, mp);
        define_global(i, "Map", ctor);

        let sp = i.new_obj();
        method(i, &sp, "add", set_add);
        method(i, &sp, "has", set_has);
        method(i, &sp, "delete", set_delete);
        method(i, &sp, "clear", set_clear);
        method(i, &sp, "forEach", set_for_each);
        method(i, &sp, "keys", set_values);
        method(i, &sp, "values", set_values);
        method(i, &sp, "entries", set_entries);
        let sv = i.obj_get_raw_str(&sp, "values").unwrap();
        sp.borrow_mut()
            .props
            .insert(PropKey::Sym(SYM_ITERATOR), Prop::builtin(sv));
        let ssize = native(i, "get size", set_size);
        sp.borrow_mut()
            .props
            .insert(pk("size"), Prop::accessor(Some(ssize), None));
        let sctor = make_ctor(i, "Set", set_ctor, sp);
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
        ("encodeURI", g_identity_str),
        ("decodeURI", g_identity_str),
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
        method(i, &wp, "get", map_get);
        method(i, &wp, "set", map_set);
        method(i, &wp, "has", map_has);
        method(i, &wp, "delete", map_delete);
        let wctor = make_ctor(i, "WeakMap", map_ctor, wp);
        define_global(i, "WeakMap", wctor);
        let wsp = i.new_obj();
        method(i, &wsp, "add", set_add);
        method(i, &wsp, "has", set_has);
        method(i, &wsp, "delete", set_delete);
        let wsctor = make_ctor(i, "WeakSet", set_ctor, wsp);
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
        define_global(i, "Reflect", Value::Obj(r));
    }

    // binary views + web-platform surface (TextEncoder, URL, Blob, …)
    crate::binary::install(i);
}

fn make_ctor(i: &mut Interp, name: &str, f: NativeFn, proto: ObjRef) -> Value {
    let o = Rc::new(RefCell::new(Obj::new(
        Some(i.p.func.clone()),
        ObjKind::Func(Func::Native {
            name: Rc::from(name),
            f,
        }),
    )));
    i.set_attr(&o, "name", Value::Str(Rc::from(name)), false, true);
    i.set_attr(&o, "length", Value::Num(0.0), false, true);
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
    let target = arg(args, 0);
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
            let d =
                obj_get_own_prop_desc(i, &Value::Und, &[this.clone(), kv])?;
            return Ok(Value::Bool(!matches!(d, Value::Und)));
        }
        match &b.kind {
            ObjKind::Arr(items) => {
                if let PropKey::S(s) = &k {
                    if s.as_ref() == "length" {
                        return Ok(Value::Bool(true));
                    }
                    if let Some(idx) = arr_index(s) {
                        if idx < items.len()
                            && !b.holes.as_ref().is_some_and(|h| h.contains(&idx))
                        {
                            return Ok(Value::Bool(true));
                        }
                    }
                }
            }
            ObjKind::Typed { buf, off, len, kind, track } => {
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
                        if idx < items.len()
                            && !b.holes.as_ref().is_some_and(|h| h.contains(&idx))
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
    let _ = i;
    let a = arg(args, 0);
    if let (Value::Obj(proto), Value::Obj(c0)) = (this, &a) {
        let mut cur = c0.clone();
        for _ in 0..10_000 {
            let next = cur.borrow().proto.clone();
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
        if let Some(p) = o.borrow().props.get(&k) {
            return Ok(Value::Bool(p.enumerable));
        }
        let b = o.borrow();
        if let ObjKind::Arr(items) = &b.kind {
            if let PropKey::S(s) = &k {
                if let Some(idx) = arr_index(s) {
                    return Ok(Value::Bool(
                        idx < items.len()
                            && !b.holes.as_ref().is_some_and(|h| h.contains(&idx)),
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
                        idx < items.len()
                            && !b.holes.as_ref().is_some_and(|h| h.contains(&idx)),
                    ));
                }
            }
        }
        // integer-indexed exotic slots are enumerable (10.4.5.3) — only
        // live, in-bounds canonical numeric keys produce a descriptor
        if let ObjKind::Typed { buf, off, len, kind, track } = &b.kind {
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
    let tag = match this {
        Value::Und => "Undefined",
        Value::Null => "Null",
        Value::Num(_) => "Number",
        Value::Str(_) => "String",
        Value::Bool(_) => "Boolean",
        Value::Sym(..) => "Symbol",
        Value::Obj(o) => {
            let builtin = match &o.borrow().kind {
                ObjKind::Arr(_) => "Array",
                ObjKind::Func(_) | ObjKind::Bound { .. } => "Function",
                ObjKind::Err => "Error",
                ObjKind::Promise(_) => "Promise",
                ObjKind::Map(_) => "Map",
                ObjKind::Set(_) => "Set",
                ObjKind::Bytes(_) => "ArrayBuffer",
                ObjKind::Typed { kind, .. } => kind.type_name(),
                ObjKind::View { .. } => "DataView",
                _ => "Object",
            };
            // spec: a string-valued @@toStringTag overrides the builtin tag
            if let Value::Str(t) =
                i.get_prop(this, &PropKey::Sym(SYM_TOSTRINGTAG))?
            {
                return Ok(Value::Str(
                    Rc::from(format!("[object {t}]").as_str()),
                ));
            }
            builtin
        }
    };
    Ok(Value::Str(Rc::from(format!("[object {tag}]").as_str())))
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
            if let ObjKind::Proxy { target, handler } = &o.borrow().kind {
                let (t, h) = (target.clone(), handler.clone());
                return match i.proxy_trap(&h, "getPrototypeOf")? {
                    Some(trap) => {
                        let r = i.call(&trap, h, vec![t])?;
                        match r {
                            Value::Obj(_) | Value::Null => Ok(r),
                            _ => Err(i.type_err(
                                "proxy getPrototypeOf must return object or null",
                            )),
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
        Value::Und | Value::Null => {
            Err(i.type_err("Cannot convert undefined or null to object"))
        }
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
    // spec __proto__ setter: primitives + a non-object proto are
    // silently ignored; a [[SetPrototypeOf]] failure throws TypeError
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
fn obj_define_accessor(
    i: &mut Interp,
    this: &Value,
    args: &[Value],
    field: &str,
) -> NResult {
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
    let Value::Obj(o) = &o else {
        return Ok(Value::Und);
    };
    if !define_prop_from_desc(i, o, k, &Value::Obj(d))? {
        return Err(i.type_err("Cannot define property"));
    }
    Ok(Value::Und)
}

fn obj_define_getter(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    obj_define_accessor(i, this, args, "get")
}

fn obj_define_setter(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    obj_define_accessor(i, this, args, "set")
}

/// Annex B `__lookupGetter__`/`__lookupSetter__` — first accessor own
/// prop up the chain answers; a data own prop stops the walk.
fn obj_lookup_accessor(
    i: &mut Interp,
    this: &Value,
    args: &[Value],
    getter: bool,
) -> NResult {
    let o = i.to_object(this)?;
    let k = i.to_propkey(&arg(args, 0))?;
    let mut cur = match &o {
        Value::Obj(o) => Some(o.clone()),
        _ => None,
    };
    while let Some(c) = cur {
        let (hit, next) = {
            let b = c.borrow();
            // a live dense element is a data own prop — it stops the
            // lookup even though it never lives in `props`
            let dense = if let PropKey::S(s) = &k {
                arr_index(s).is_some_and(|idx| {
                    let items = match &b.kind {
                        ObjKind::Arr(items) => Some(items.len()),
                        ObjKind::Args { items, .. } => Some(items.len()),
                        _ => None,
                    };
                    items.is_some_and(|n| {
                        idx < n
                            && !b.holes.as_ref().is_some_and(|h| h.contains(&idx))
                    })
                })
            } else {
                false
            };
            if dense {
                (Some(Value::Und), None)
            } else {
                match b.props.get(&k) {
                    Some(p) if p.is_accessor() => (
                        Some(if getter {
                            p.get.clone().unwrap_or(Value::Und)
                        } else {
                            p.set.clone().unwrap_or(Value::Und)
                        }),
                        None,
                    ),
                    Some(_) => (Some(Value::Und), None),
                    None => (None, b.proto.clone()),
                }
            }
        };
        if let Some(v) = hit {
            return Ok(v);
        }
        cur = next;
    }
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
    let keys = own_enum_entries_full(i, &v)?
        .into_iter()
        .filter_map(|(k, _)| match k {
            PropKey::S(s) => Some(Value::Str(s)),
            _ => None, // string keys only
        })
        .collect();
    Ok(Value::Obj(i.new_arr(keys)))
}

fn obj_values(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let v = i.to_object(&arg(args, 0))?;
    let out: Vec<Value> = own_enum_entries_full(i, &v)?
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
    for (k, val) in own_enum_entries_full(i, &v)? {
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
        for (k, v) in own_enum_entries_full(i, &src)? {
            i.set_prop(&target, k, v)?;
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
    if !matches!(desc, Value::Obj(_)) {
        return Err(i.type_err("Property description must be an object"));
    }
    if let ObjKind::Proxy { target: tg, handler } = o.borrow().kind.clone() {
        let (tg, h) = (tg, handler);
        return match i.proxy_trap(&h, "defineProperty")? {
            Some(trap) => {
                let r =
                    i.call(&trap, h, vec![tg, i.key_to_value(&key), desc])?;
                if !i.to_bool(&r) {
                    return Err(i.type_err(
                        "proxy defineProperty trap returned falsy",
                    ));
                }
                Ok(target)
            }
            None => match &tg {
                Value::Obj(to) => {
                    if !define_prop_from_desc(i, to, key, &arg(args, 2))? {
                        return Err(i.type_err("Cannot define property"));
                    }
                    Ok(target)
                }
                _ => Err(i.type_err("proxy target is not an object")),
            },
        };
    }
    if !define_prop_from_desc(i, &o, key, &desc)? {
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
            ObjKind::Typed { buf, off, len, kind, track } => {
                (buf.clone(), *off, *len, *kind, *track)
            }
            _ => unreachable!(),
        };
        if !typed_state(&buf, off, len, kind, track)
            .is_some_and(|eff| valid_int_index(ni, eff))
        {
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
            let n = crate::binary::ta_elem_write(
                i,
                &Value::Obj(o.clone()),
                &vv,
            )?;
            // coercion ran side-effects — revalidate before writing so a
            // detach/resize during valueOf drops the write (the define
            // still reports success, spec IntegerIndexedElementSet)
            if typed_state(&buf, off, len, kind, track)
                .is_some_and(|eff| valid_int_index(ni, eff))
            {
                if buf.borrow().immutable {
                    return Err(i.type_err(
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
        let args_hole = args_idx
            .is_some_and(|i| b.holes.as_ref().is_some_and(|h| h.contains(&i)));
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
                        let mapped_val = if let (Some(e), Some(Some(name))) =
                            (&*env, map.get(idx))
                        {
                            e.vars
                                .borrow()
                                .get(name.as_ref())
                                .map(|b| b.val.clone())
                        } else {
                            None
                        };
                        // spec 10.4.4.13: an accessor descriptor or
                        // writable:false severs the param link — the
                        // current env value becomes the stored value;
                        // configurable:false does NOT unmap
                        if was_mapped
                            && (is_acc || (has_w && !i.to_bool(&wv)))
                        {
                            if let Some(m) = map.get_mut(idx) {
                                *m = None;
                            }
                        }
                        // still mapped + value → keep the binding in sync
                        if has_val {
                            items[idx] = vv.clone();
                            if matches!(map.get(idx), Some(Some(_))) {
                                if let (Some(e), Some(Some(name))) =
                                    (&*env, map.get(idx))
                                {
                                    let name = name.clone();
                                    if let Some(bnd) =
                                        e.vars.borrow_mut().get_mut(name.as_ref())
                                    {
                                        bnd.val = vv.clone();
                                    }
                                }
                            }
                        }
                        // materialize merged attrs so descriptor
                        // semantics apply to the slot
                        let cur_val =
                            mapped_val.unwrap_or_else(|| items[idx].clone());
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
            } else if matches!(&b.kind, ObjKind::Arr(_)) && b.extensible
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
                    let hole =
                        b.holes.as_ref().is_some_and(|h| h.contains(&idx));
                    let in_range =
                        matches!(&b.kind, ObjKind::Arr(it) if idx < it.len());
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
                                    Value::Num(n) => {
                                        (*n as usize).max(items_len)
                                    }
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
                        } else if pure_val && !hole && b.extensible
                            && idx <= 8_000_000
                        {
                            // fresh index within dense range — grow the
                            // store; the gap is holes, not undefined
                            // elements; length follows items.len()
                            let mut grow_from = None;
                            if let ObjKind::Arr(items) = &mut b.kind {
                                let old = items.len();
                                items.resize(idx + 1, Value::Und);
                                if idx > old {
                                    grow_from = Some(old);
                                }
                                items[idx] = vv.clone();
                            }
                            if let Some(old) = grow_from {
                                let h = b.holes.get_or_insert_with(Default::default);
                                for k in old..idx {
                                    h.insert(k);
                                }
                            }
                            return Ok(true);
                        }
                        // everything else (sparse beyond the cap, holed
                        // slots, attr-bearing descs, non-extensible
                        // targets) goes through the generic path; the
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
                                Value::Num(n) => {
                                    (*n as usize).max(items_len)
                                }
                                _ => items_len,
                            };
                            (n, p.writable)
                        }
                        None => (items_len, true),
                    };
                    // length is {enumerable:false, configurable:false}
                    // — attr flips to true are rejected outright
                    if (has_c && i.to_bool(&cv))
                        || (has_e && i.to_bool(&ev))
                    {
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
                                    PropKey::S(s) => {
                                        js_int_index(s.as_ref())
                                            .filter(|ix| {
                                                *ix >= nl && *ix < eff_len
                                            })
                                            .map(|ix| (ix, k.clone()))
                                    }
                                    _ => None,
                                })
                                .collect();
                            let fail_at = doomed
                                .iter()
                                .filter(|(_, k)| {
                                    !b.props.get(k).map(|p| p.configurable)
                                        .unwrap_or(true)
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
                                                val: Value::Num(
                                                    (k + 1) as f64,
                                                ),
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
            if is_acc != old.is_accessor() {
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
        if has_get {
            prop.get = if matches!(gv, Value::Und) { None } else { Some(gv) };
        }
        if has_set {
            prop.set = if matches!(sv, Value::Und) { None } else { Some(sv) };
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
                    let in_range =
                        matches!(&b.kind, ObjKind::Arr(it) if idx < it.len());
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
                                    freeze = e
                                        .vars
                                        .borrow()
                                        .get(name.as_ref())
                                        .map(|x| x.val.clone());
                                }
                                if let Some(m) = map.get_mut(idx) {
                                    *m = None;
                                }
                            }
                        }
                        if has_val {
                            items[idx] = vv.clone();
                            if let (Some(e), Some(Some(name))) =
                                (&*env, map.get(idx))
                            {
                                let name = name.clone();
                                if let Some(bnd) =
                                    e.vars.borrow_mut().get_mut(name.as_ref())
                                {
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
    for (k, d) in own_enum_entries_full(i, &descs)? {
        // ToPropertyDescriptor requires an object — primitives throw
        if !matches!(d, Value::Obj(_)) {
            return Err(i.type_err("Property description must be an object"));
        }
        // route through Object.defineProperty so a proxy target fires
        // its defineProperty trap per key
        let kv = i.key_to_value(&k);
        obj_define_property(i, &Value::Und, &[target.clone(), kv, d])?;
    }
    Ok(target)
}

fn obj_get_own_prop_desc(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let v = i.to_object(&arg(args, 0))?;
    let key = i.to_propkey(&arg(args, 1))?;
    let Value::Obj(o) = &v else {
        return Ok(Value::Und);
    };
    if let ObjKind::Proxy { target, handler } = &o.borrow().kind {
        let (t, h) = (target.clone(), handler.clone());
        return match i.proxy_trap(&h, "getOwnPropertyDescriptor")? {
            Some(trap) => {
                let r = i.call(&trap, h, vec![t, i.key_to_value(&key)])?;
                match r {
                    Value::Und => Ok(Value::Und),
                    Value::Obj(_) => {
                        // spec ToPropertyDescriptor + FromPropertyDescriptor:
                        // the six fields are read through Get (getters run,
                        // accessor/data conflicts throw) and a normalized
                        // fresh descriptor object is returned
                        let (hg, gv) = read_desc_prop(i, &r, "get")?;
                        let (hs, sv) = read_desc_prop(i, &r, "set")?;
                        let (hv, vv) = read_desc_prop(i, &r, "value")?;
                        let (hw, wv) = read_desc_prop(i, &r, "writable")?;
                        let (he, ev) = read_desc_prop(i, &r, "enumerable")?;
                        let (hc, cv) = read_desc_prop(i, &r, "configurable")?;
                        if (hg || hs) && (hv || hw) {
                            return Err(i.type_err(
                                "Invalid property descriptor. Cannot both specify accessors and a value or writable attribute",
                            ));
                        }
                        if hg && !matches!(gv, Value::Und) && !i.is_callable(&gv)
                        {
                            return Err(i.type_err("Getter must be a function"));
                        }
                        if hs && !matches!(sv, Value::Und) && !i.is_callable(&sv)
                        {
                            return Err(i.type_err("Setter must be a function"));
                        }
                        let d = i.new_obj();
                        if hv {
                            i.set_data(&d, "value", vv);
                        }
                        if hw {
                            i.set_data(&d, "writable", wv);
                        }
                        if hg {
                            i.set_data(&d, "get", gv);
                        }
                        if hs {
                            i.set_data(&d, "set", sv);
                        }
                        if he {
                            i.set_data(&d, "enumerable", ev);
                        }
                        if hc {
                            i.set_data(&d, "configurable", cv);
                        }
                        Ok(Value::Obj(d))
                    }
                    _ => Err(i.type_err(
                        "proxy gOPD trap must return object or undefined",
                    )),
                }
            }
            None => obj_get_own_prop_desc(i, &Value::Und, &[t, arg(args, 1)]),
        };
    }
    // arguments-object index — mapped slots describe the live binding
    {
        let b = o.borrow();
        if let ObjKind::Args { items, env, map } = &b.kind {
            if let PropKey::S(s) = &key {
                if let Some(idx) = arr_index(s) {
                    if idx < items.len()
                        && !b.holes.as_ref().is_some_and(|h| h.contains(&idx))
                    {
                        let mapped_val = if let (Some(e), Some(Some(name))) =
                            (env, map.get(idx))
                        {
                            e.vars
                                .borrow()
                                .get(name.as_ref())
                                .map(|b| b.val.clone())
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
                    ObjKind::Typed { buf, off, len, kind, track } => {
                        (buf.clone(), *off, *len, *kind, *track)
                    }
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
                let n = kind.read(
                    &bytes.borrow(),
                    off + ni as usize * kind.size(),
                );
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
        let d = obj_get_own_prop_desc(
            i,
            &Value::Und,
            &[v.clone(), i.key_to_value(&k)],
        )?;
        let ov = Value::Obj(out.clone());
        i.set_prop(&ov, k, d)?;
    }
    Ok(Value::Obj(out))
}

fn obj_get_prototype_of(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    // spec ToObject: null/undefined throw; primitives read their
    // wrapper's proto (Number.prototype &co)
    let v = i.to_object(&arg(args, 0))?;
    if let Value::Obj(o) = &v {
        if let ObjKind::Proxy { target, handler } = &o.borrow().kind {
            let (t, h) = (target.clone(), handler.clone());
            return match i.proxy_trap(&h, "getPrototypeOf")? {
                Some(trap) => {
                    let r = i.call(&trap, h, vec![t])?;
                    match r {
                        Value::Obj(_) | Value::Null => Ok(r),
                        _ => Err(i.type_err(
                            "proxy getPrototypeOf must return object or null",
                        )),
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
    if let ObjKind::Proxy { target, handler } = o.borrow().kind.clone() {
        return match i.proxy_trap(&handler, "setPrototypeOf")? {
            Some(trap) => {
                let r = i.call(&trap, handler, vec![target, v.clone()])?;
                Ok(i.to_bool(&r))
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
        return Err(
            i.type_err("Object prototype may only be an Object or null")
        );
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
        for (k, d) in own_enum_entries_full(i, &descs)? {
            if !matches!(d, Value::Obj(_)) {
                return Err(
                    i.type_err("Property description must be an object")
                );
            }
            define_prop_from_desc(i, &o, k, &d)?;
        }
    }
    Ok(Value::Obj(o))
}

/// `[[IsExtensible]]` as a boolean — proxies fire the `isExtensible` trap.
fn ord_is_extensible(i: &mut Interp, o: &ObjRef) -> Result<bool, Value> {
    if let ObjKind::Proxy { target, handler } = o.borrow().kind.clone() {
        return match i.proxy_trap(&handler, "isExtensible")? {
            Some(trap) => {
                let r = i.call(&trap, handler, vec![target])?;
                Ok(i.to_bool(&r))
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
    if let ObjKind::Proxy { target, handler } = o.borrow().kind.clone() {
        return match i.proxy_trap(&handler, "preventExtensions")? {
            Some(trap) => {
                let r = i.call(&trap, handler, vec![target])?;
                Ok(i.to_bool(&r))
            }
            None => match target {
                Value::Obj(to) => ord_prevent_ext(i, &to),
                _ => Ok(false),
            },
        };
    }
    // 10.4.5.4: a length-tracking TA or one with live elements can never
    // become non-extensible — [[PreventExtensions]] fails outright
    {
        let b = o.borrow();
        if let ObjKind::Typed { buf, off, len, kind, track } = &b.kind {
            if *track {
                return Ok(false);
            }
            if typed_state(buf, *off, *len, *kind, *track)
                .is_some_and(|eff| eff > 0)
            {
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
                ObjKind::Arr(items) | ObjKind::Args { items, .. } => {
                    items[idx].clone()
                }
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
fn set_integrity(i: &mut Interp, o: &ObjRef, frozen: bool) -> Result<bool, Value> {
    let ov = Value::Obj(o.clone());
    if !ord_prevent_ext(i, o)? {
        return Ok(false);
    }
    if matches!(&o.borrow().kind, ObjKind::Proxy { .. }) {
        for k in own_keys_all(i, &ov)? {
            let d = i.new_obj();
            i.set_data(&d, "configurable", Value::Bool(false));
            if frozen {
                i.set_data(&d, "writable", Value::Bool(false));
            }
            obj_define_property(
                i,
                &Value::Und,
                &[ov.clone(), i.key_to_value(&k), Value::Obj(d)],
            )?;
        }
        return Ok(true);
    }
    {
        let b = o.borrow();
        if let ObjKind::Typed { buf, off, len, kind, track } = &b.kind {
            if typed_state(buf, *off, *len, *kind, *track)
                .is_some_and(|eff| eff > 0)
            {
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
    if matches!(&o.borrow().kind, ObjKind::Proxy { .. }) {
        let ov = Value::Obj(o.clone());
        for k in own_keys_all(i, &ov)? {
            let d = obj_get_own_prop_desc(
                i,
                &Value::Und,
                &[ov.clone(), i.key_to_value(&k)],
            )?;
            if let Value::Obj(d) = d {
                let dv = Value::Obj(d);
                let cfg = i.get_prop_str(&dv, "configurable")?;
                if i.to_bool(&cfg) {
                    return Ok(false);
                }
                if frozen {
                    let g = i.get_prop_str(&dv, "get")?;
                    let s = i.get_prop_str(&dv, "set")?;
                    let is_acc = !(matches!(g, Value::Und)
                        && matches!(s, Value::Und));
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
    if let ObjKind::Typed { buf, off, len, kind, track } = &b.kind {
        if typed_state(buf, *off, *len, *kind, *track)
            .is_some_and(|eff| eff > 0)
        {
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
    let items = i.iter_values(&arg(args, 0))?;
    for e in items {
        let k = i.get_prop_str(&e, "0")?;
        let v = i.get_prop_str(&e, "1")?;
        let key = i.to_propkey(&k)?;
        i.set_prop(&Value::Obj(o.clone()), key, v)?;
    }
    Ok(Value::Obj(o))
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
        // box primitives
        other => {
            let proto = match &other {
                Value::Str(_) => i.p.string.clone(),
                Value::Num(_) => i.p.number.clone(),
                Value::Bool(_) => i.p.boolean.clone(),
                Value::Sym(..) => i.p.symbol.clone(),
                _ => i.p.obj.clone(),
            };
            let o = Rc::new(RefCell::new(Obj::new(Some(proto), ObjKind::Ord)));
            i.set_builtin(&o, "$value", other);
            Ok(Value::Obj(o))
        }
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
    // spec: name = "bound " + target.name; length = max(0, L - N) where L
    // is the target's .length and N the bound-arg count
    let tname = match i.get_prop_str(this, "name") {
        Ok(Value::Str(s)) => s.to_string(),
        _ => String::new(),
    };
    i.set_attr(&bound, "name", Value::Str(Rc::from(format!("bound {tname}"))), false, true);
    let tlen = match i.get_prop_str(this, "length") {
        Ok(v) => i.to_num(&v).unwrap_or(0.0),
        Err(_) => 0.0,
    };
    let blen = (tlen - (args.len() as f64 - 1.0).max(0.0)).max(0.0);
    i.set_attr(&bound, "length", Value::Num(blen), false, true);
    Ok(Value::Obj(bound))
}

/// 7.3.19 OrdinaryHasInstance(C, O) — bound functions unwrap to their
/// target; non-callable C → false; `C.prototype` non-object → TypeError.
pub(crate) fn ordinary_has_instance(
    i: &mut Interp,
    c: &Value,
    o: &Value,
) -> Result<bool, Value> {
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
        let next = cur_o.borrow().proto.clone();
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

fn fn_to_string(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let _ = i;
    let name = if let Value::Obj(o) = this {
        match &o.borrow().kind {
            ObjKind::Func(Func::User { def, .. }) => def.name.clone().unwrap_or_default(),
            ObjKind::Func(Func::Native { name, .. }) => name.to_string(),
            _ => String::new(),
        }
    } else {
        String::new()
    };
    Ok(Value::Str(Rc::from(
        format!("function {name}() {{ [native code] }}").as_str(),
    )))
}

// ===========================================================================
// natives: array
// ===========================================================================

fn array_ctor(i: &mut Interp, _this: &Value, args: &[Value]) -> NResult {
    // Array(n) → length-n array of holes; Array(a,b,..) → [a,b,..]
    if args.len() == 1 {
        if let Value::Num(n) = args[0] {
            if n >= 0.0 && n == n.trunc() && n < 4294967295.0 {
                // dense Vec backing — can't honor holey arrays past a
                // sane bound (spec allows 2^32-1; real engines go sparse)
                if n > 8_000_000.0 {
                    return Err(i.range_err("Invalid array length"));
                }
                return Ok(Value::Obj(i.new_arr(vec![Value::Und; n as usize])));
            }
            return Err(i.range_err("Invalid array length"));
        }
    }
    Ok(Value::Obj(i.new_arr(args.to_vec())))
}

fn array_is_array(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let _ = i;
    Ok(Value::Bool(matches!(
        &arg(args, 0),
        Value::Obj(o) if matches!(o.borrow().kind, ObjKind::Arr(_))
    )))
}

fn array_from(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let src = arg(args, 0);
    if matches!(src, Value::Und | Value::Null) {
        return Err(i.type_err("Array.from requires an array-like or iterable"));
    }
    let items = match i.iter_values(&src) {
        Ok(v) => v,
        Err(_) => to_elems(i, &src)?,
    };
    let map_fn = arg(args, 1);
    let this_arg = arg(args, 2);
    let mut out = Vec::new();
    for (idx, it) in items.into_iter().enumerate() {
        if i.is_callable(&map_fn) {
            out.push(i.call(&map_fn, this_arg.clone(), vec![it, Value::Num(idx as f64)])?);
        } else {
            out.push(it);
        }
    }
    Ok(Value::Obj(i.new_arr(out)))
}

fn array_of(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    Ok(Value::Obj(i.new_arr(args.to_vec())))
}

fn arr_push(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let _ = i;
    if let Value::Obj(o) = this {
        let mut b = o.borrow_mut();
        if let ObjKind::Arr(items) = &b.kind {
            // spec: push starts at the *declared* length
            let declared = match b.props.get(&PropKey::from_str("length")) {
                Some(p) => match &p.val {
                    Value::Num(n) => *n as usize,
                    _ => 0,
                },
                None => 0,
            };
            let start = declared.max(items.len());
            if let ObjKind::Arr(items) = &mut b.kind {
                if start > items.len() && start <= 8_000_000 {
                    items.resize(start, Value::Und);
                }
                items.extend(args.iter().cloned());
                return Ok(Value::Num(items.len() as f64));
            }
        }
    }
    Ok(Value::Und)
}

fn arr_pop(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let _ = i;
    if let Value::Obj(o) = this {
        let mut b = o.borrow_mut();
        if let ObjKind::Arr(items) = &b.kind {
            let declared = match b.props.get(&PropKey::from_str("length")) {
                Some(p) => match &p.val {
                    Value::Num(n) => *n as usize,
                    _ => 0,
                },
                None => 0,
            };
            let len = declared.max(items.len());
            if len == 0 {
                return Ok(Value::Und);
            }
            let idx = len - 1;
            let out = if let ObjKind::Arr(items) = &mut b.kind {
                if idx < items.len() {
                    let v = items.get(idx).cloned().unwrap_or(Value::Und);
                    items.truncate(idx);
                    v
                } else {
                    Value::Und // trailing hole
                }
            } else {
                Value::Und
            };
            if let Some(p) = b.props.get_mut(&PropKey::from_str("length")) {
                p.val = Value::Num(idx as f64);
            }
            return Ok(out);
        }
    }
    Ok(Value::Und)
}

fn arr_shift(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let _ = i;
    let mut out = Value::Und;
    with_arr(this, |items| {
        if !items.is_empty() {
            out = items.remove(0);
        }
    });
    Ok(out)
}

fn arr_unshift(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let _ = i;
    let mut len = 0;
    with_arr(this, |items| {
        for (k, v) in args.iter().enumerate() {
            items.insert(k, v.clone());
        }
        len = items.len();
    });
    Ok(Value::Num(len as f64))
}

fn arr_slice(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec: ToObject → length → bounds → ArraySpeciesCreate(O, count) →
    // copy only PRESENT indices (HasProperty)
    let len = elems_len(i, this)?;
    let start = clamp_index(i.to_num(&arg(args, 0)).unwrap_or(0.0), len);
    let end = match arg(args, 1) {
        Value::Und => len,
        v => clamp_index(i.to_num(&v).unwrap_or(0.0), len),
    };
    let a = species_create(i, this, end.saturating_sub(start))?;
    let items = to_elems_sparse(i, this)?;
    let mut n = 0usize;
    for (k, v) in items {
        if k >= start && k < end {
            create_data_prop(i, &a, n, v)?;
            n += 1;
        }
    }
    Ok(a)
}

fn arr_splice(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec order: length/bounds coercions (user code) → removed array via
    // ArraySpeciesCreate → element read → mutation of `this`.
    let len = elems_len(i, this)?;
    let start = clamp_index(i.to_num(&arg(args, 0)).unwrap_or(0.0), len);
    let del = match args.get(1) {
        None => len - start,
        Some(v) => {
            let n = i.to_num(v).unwrap_or(0.0);
            if n < 0.0 {
                0
            } else {
                (n as usize).min(len - start)
            }
        }
    };
    let items = to_elems_sparse(i, this)?;
    let a = species_create(i, this, del)?;
    for (k, v) in &items {
        if *k >= start && *k < start + del {
            create_data_prop(i, &a, k - start, v.clone())?;
        }
    }
    let insert: Vec<Value> = args.iter().skip(2).cloned().collect();
    // dense fast path — no user code below
    if let Value::Obj(o) = this {
        if let ObjKind::Arr(elems) = &mut o.borrow_mut().kind {
            if start <= elems.len() {
                let tail = elems.split_off(start);
                let rest = tail[del.min(tail.len())..].to_vec();
                for (k, v) in insert.iter().enumerate() {
                    elems.insert(start + k, v.clone());
                }
                elems.extend(rest);
            }
            return Ok(a);
        }
    }
    // generic receiver: rebuild indexed props + length
    let new_len = len - del + insert.len();
    let mut nv: Vec<Option<Value>> = vec![None; new_len];
    for (k, v) in items {
        if k < start {
            nv[k] = Some(v);
        } else if k >= start + del {
            let nk = k - del + insert.len();
            if nk < nv.len() {
                nv[nk] = Some(v);
            }
        }
    }
    for (k, v) in insert.iter().enumerate() {
        if start + k < nv.len() {
            nv[start + k] = Some(v.clone());
        }
    }
    for (k, v) in nv.iter().enumerate() {
        match v {
            Some(v) => {
                i.set_prop(this, PropKey::from_str(&k.to_string()), v.clone())?
            }
            None => {
                i.delete_prop(this, &PropKey::from_str(&k.to_string()))?;
            }
        }
    }
    for k in new_len..len {
        i.delete_prop(this, &PropKey::from_str(&k.to_string()))?;
    }
    i.set_prop(
        this,
        PropKey::from_str("length"),
        Value::Num(new_len as f64),
    )?;
    Ok(a)
}

fn arr_concat(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    // spec: E = ToObject(this) — primitives box to wrappers; A =
    // ArraySpeciesCreate(O,0); each element of [E, ...args] spreads iff
    // IsConcatSpreadable.
    let boxed = i.to_object(this)?;
    let a = species_create(i, this, 0)?;
    let mut n = 0usize;
    for e in std::iter::once(&boxed).chain(args.iter()) {
        let spreadable = match e {
            Value::Obj(o) => {
                let flag =
                    i.get_prop(e, &PropKey::Sym(SYM_IS_CONCAT_SPREADABLE))?;
                if matches!(flag, Value::Und) {
                    matches!(o.borrow().kind, ObjKind::Arr(_))
                } else {
                    i.to_bool(&flag)
                }
            }
            _ => false,
        };
        if spreadable {
            // spread visits only PRESENT indices (HasProperty)
            for (_, v) in to_elems_sparse(i, e)? {
                create_data_prop(i, &a, n, v)?;
                n += 1;
            }
        } else {
            create_data_prop(i, &a, n, e.clone())?;
            n += 1;
        }
    }
    Ok(a)
}

fn arr_join(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let sep = match arg(args, 0) {
        Value::Und => ",".to_string(),
        v => i.to_str(&v)?,
    };
    // snapshot the items: to_str on an element can run user code that
    // mutates the array, so we can't hold the borrow across it — but we
    // can append straight into the output instead of a Vec<String> + join.
    let items = to_elems(i, this)?;
    let mut out = String::new();
    for (k, it) in items.iter().enumerate() {
        if k > 0 {
            out.push_str(&sep);
        }
        match it {
            Value::Und | Value::Null => {}
            Value::Str(s) => out.push_str(s),
            Value::Num(n) => out.push_str(&js_num_str(*n)),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            v => out.push_str(&i.to_str(v)?),
        }
    }
    Ok(Value::Str(Rc::from(out.as_str())))
}

fn arr_index_of(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let needle = arg(args, 0);
    // numeric/absent fromIndex can't run user code — scan under one borrow
    if matches!(arg(args, 1), Value::Num(_) | Value::Und) {
        if let Value::Obj(o) = this {
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
    let len = elems_len(i, this)?;
    let start = clamp_index(i.to_num(&arg(args, 1)).unwrap_or(0.0), len);
    // skip missing properties — HasProperty, not Get
    let items = to_elems_sparse(i, this)?;
    for (k, v) in items {
        if k < start {
            continue;
        }
        if strict_val_eq(&v, &needle) {
            return Ok(Value::Num(k as f64));
        }
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
    // fromIndex may run user code (valueOf) — convert BEFORE borrowing
    let from = match args.get(1) {
        Some(v) => Some(i.to_num(v)?),
        None => None,
    };
    // no user code runs from here — scan in place under one borrow
    if let Value::Obj(o) = this {
        let b = o.borrow();
        if let ObjKind::Arr(items) = &b.kind {
            let start = match from {
                Some(n) => last_index_start(n, items.len()),
                None => items.len().checked_sub(1),
            };
            let Some(start) = start else {
                return Ok(Value::Num(-1.0));
            };
            for k in (0..=start).rev() {
                if strict_val_eq(&items[k], &needle) {
                    return Ok(Value::Num(k as f64));
                }
            }
            return Ok(Value::Num(-1.0));
        }
    }
    let len = elems_len(i, this)?;
    let start = match from {
        Some(n) => last_index_start(n, len),
        None => len.checked_sub(1),
    };
    let Some(start) = start else {
        return Ok(Value::Num(-1.0));
    };
    // skip missing properties — HasProperty, not Get
    let items = to_elems_sparse(i, this)?;
    for (k, v) in items.iter().rev() {
        if *k > start {
            continue;
        }
        if strict_val_eq(v, &needle) {
            return Ok(Value::Num(*k as f64));
        }
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
        (Value::Obj(x), Value::Obj(y)) => Rc::ptr_eq(x, y),
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
    // no user code runs here — scan in place under one borrow
    if let Value::Obj(o) = this {
        let b = o.borrow();
        if let ObjKind::Arr(items) = &b.kind {
            return Ok(Value::Bool(items.iter().any(|v| same_value_zero(v, &needle))));
        }
    }
    let items = to_elems(i, this)?;
    Ok(Value::Bool(items.iter().any(|v| same_value_zero(v, &needle))))
}

fn arr_find(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    let items = to_elems_sparse(i, this)?;
    for (k, v) in items.iter() {
        let r = i.call(
            &cb,
            this_arg.clone(),
            vec![v.clone(), Value::Num(*k as f64), this.clone()],
        )?;
        if i.to_bool(&r) {
            return Ok(v.clone());
        }
    }
    Ok(Value::Und)
}

fn arr_find_index(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    let items = to_elems_sparse(i, this)?;
    for (k, v) in items.iter() {
        let r = i.call(
            &cb,
            this_arg.clone(),
            vec![v.clone(), Value::Num(*k as f64), this.clone()],
        )?;
        if i.to_bool(&r) {
            return Ok(Value::Num(*k as f64));
        }
    }
    Ok(Value::Num(-1.0))
}

fn arr_find_last(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    let items = to_elems_sparse(i, this)?;
    for (k, v) in items.iter().rev() {
        let r = i.call(
            &cb,
            this_arg.clone(),
            vec![v.clone(), Value::Num(*k as f64), this.clone()],
        )?;
        if i.to_bool(&r) {
            return Ok(v.clone());
        }
    }
    Ok(Value::Und)
}

fn arr_find_last_index(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    let items = to_elems_sparse(i, this)?;
    for (k, v) in items.iter().rev() {
        let r = i.call(
            &cb,
            this_arg.clone(),
            vec![v.clone(), Value::Num(*k as f64), this.clone()],
        )?;
        if i.to_bool(&r) {
            return Ok(Value::Num(*k as f64));
        }
    }
    Ok(Value::Num(-1.0))
}

fn arr_filter(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    let items = to_elems_sparse(i, this)?;
    let a = species_create(i, this, 0)?;
    let mut n = 0usize;
    for (k, v) in items {
        let r = i.call(
            &cb,
            this_arg.clone(),
            vec![v.clone(), Value::Num(k as f64), this.clone()],
        )?;
        if i.to_bool(&r) {
            create_data_prop(i, &a, n, v)?;
            n += 1;
        }
    }
    Ok(a)
}

fn arr_map(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    let items = to_elems_sparse(i, this)?;
    let len = elems_len(i, this)?;
    let a = species_create(i, this, len)?;
    for (k, v) in items {
        let r = i.call(
            &cb,
            this_arg.clone(),
            vec![v, Value::Num(k as f64), this.clone()],
        )?;
        create_data_prop(i, &a, k, r)?;
    }
    Ok(a)
}

fn arr_for_each(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    let items = to_elems_sparse(i, this)?;
    for (k, v) in items {
        i.call(
            &cb,
            this_arg.clone(),
            vec![v, Value::Num(k as f64), this.clone()],
        )?;
    }
    Ok(Value::Und)
}

fn arr_reduce(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let items = to_elems_sparse(i, this)?;
    let mut it = items.into_iter();
    let mut acc = match args.get(1) {
        Some(v) => v.clone(),
        None => match it.next() {
            Some((_, v)) => v,
            None => {
                return Err(i.type_err("Reduce of empty array with no initial value"))
            }
        },
    };
    for (k, v) in it {
        acc = i.call(
            &cb,
            Value::Und,
            vec![acc, v, Value::Num(k as f64), this.clone()],
        )?;
    }
    Ok(acc)
}

fn arr_reduce_right(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let items = to_elems_sparse(i, this)?;
    let mut it = items.into_iter().rev();
    let mut acc = match args.get(1) {
        Some(v) => v.clone(),
        None => match it.next() {
            Some((_, v)) => v,
            None => {
                return Err(i.type_err("Reduce of empty array with no initial value"))
            }
        },
    };
    for (k, v) in it {
        acc = i.call(
            &cb,
            Value::Und,
            vec![acc, v, Value::Num(k as f64), this.clone()],
        )?;
    }
    Ok(acc)
}

fn arr_some(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    let items = to_elems_sparse(i, this)?;
    for (k, v) in items {
        let r = i.call(
            &cb,
            this_arg.clone(),
            vec![v, Value::Num(k as f64), this.clone()],
        )?;
        if i.to_bool(&r) {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

fn arr_every(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    let items = to_elems_sparse(i, this)?;
    for (k, v) in items {
        let r = i.call(
            &cb,
            this_arg.clone(),
            vec![v, Value::Num(k as f64), this.clone()],
        )?;
        if !i.to_bool(&r) {
            return Ok(Value::Bool(false));
        }
    }
    Ok(Value::Bool(true))
}

fn arr_flat(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let depth = match arg(args, 0) {
        Value::Und => 1,
        v => (i.to_num(&v)?.max(0.0)) as usize,
    };
    let items = to_elems_sparse(i, this)?;
    let a = species_create(i, this, 0)?;
    let mut n = 0usize;
    fn rec(
        i: &mut Interp,
        items: Vec<(usize, Value)>,
        d: usize,
        a: &Value,
        n: &mut usize,
    ) -> Result<(), Value> {
        for (_, v) in items {
            if d > 0 {
                if let Value::Obj(o) = &v {
                    if let ObjKind::Arr(inner) = &o.borrow().kind {
                        let inner: Vec<(usize, Value)> =
                            inner.iter().cloned().enumerate().collect();
                        rec(i, inner, d - 1, a, n)?;
                        continue;
                    }
                }
            }
            create_data_prop(i, a, *n, v)?;
            *n += 1;
        }
        Ok(())
    }
    rec(i, items, depth, &a, &mut n)?;
    Ok(a)
}

fn arr_flat_map(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    let items = to_elems_sparse(i, this)?;
    let a = species_create(i, this, 0)?;
    let mut n = 0usize;
    for (k, v) in items {
        let r = i.call(
            &cb,
            this_arg.clone(),
            vec![v, Value::Num(k as f64), this.clone()],
        )?;
        // FlattenIntoArray depth 1: arrays expand, others append
        if let Value::Obj(o) = &r {
            if let ObjKind::Arr(inner) = &o.borrow().kind {
                for x in inner.clone() {
                    create_data_prop(i, &a, n, x)?;
                    n += 1;
                }
                continue;
            }
        }
        create_data_prop(i, &a, n, r)?;
        n += 1;
    }
    Ok(a)
}

fn arr_fill(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let v = arg(args, 0);
    let len = arr_clone(this).len();
    let start = clamp_index(i.to_num(&arg(args, 1)).unwrap_or(0.0), len);
    let end = match arg(args, 2) {
        Value::Und => len,
        x => clamp_index(i.to_num(&x).unwrap_or(0.0), len),
    };
    with_arr(this, |items| {
        for k in start..end.min(items.len()) {
            items[k] = v.clone();
        }
    });
    Ok(this.clone())
}

fn arr_reverse(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let _ = i;
    with_arr(this, |items| items.reverse());
    Ok(this.clone())
}

fn arr_sort(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cmp = arg(args, 0);
    let mut items = arr_clone(this);
    if i.is_callable(&cmp) {
        // merge sort to guarantee stability & avoid borrow traps
        let mut err: Option<Value> = None;
        let mut work = items.clone();
        merge_sort(&mut items, &mut work, &mut |a, b| {
            if err.is_some() {
                return std::cmp::Ordering::Equal;
            }
            match i.call(&cmp, Value::Und, vec![a.clone(), b.clone()]) {
                Ok(v) => match i.to_num(&v) {
                    Ok(n) => {
                        if n < 0.0 {
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
        });
        if let Some(e) = err {
            return Err(e);
        }
    } else {
        let mut strs: Vec<(String, Value)> = Vec::new();
        for v in items.drain(..) {
            let s = match &v {
                Value::Und | Value::Null => "undefined".to_string(),
                other => i.to_str(other)?,
            };
            strs.push((s, v));
        }
        strs.sort_by(|a, b| a.0.cmp(&b.0));
        items = strs.into_iter().map(|(_, v)| v).collect();
        // undefined sorts last
        items.sort_by_key(|v| matches!(v, Value::Und));
    }
    if let Value::Obj(o) = this {
        o.borrow_mut().kind = ObjKind::Arr(items);
    }
    Ok(this.clone())
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
    // fast path: a numeric index can't run user code — read under one borrow
    if matches!(arg(args, 0), Value::Num(_)) {
        let n = i.to_num(&arg(args, 0)).unwrap_or(0.0);
        if let Value::Obj(o) = this {
            let b = o.borrow();
            if let ObjKind::Arr(items) = &b.kind {
                let idx = if n < 0.0 {
                    items.len() as i64 + n as i64
                } else {
                    n as i64
                };
                if idx < 0 || idx >= items.len() as i64 {
                    return Ok(Value::Und);
                }
                return Ok(items[idx as usize].clone());
            }
        }
        return Ok(Value::Und);
    }
    let items = to_elems(i, this)?;
    let n = i.to_num(&arg(args, 0)).unwrap_or(0.0);
    let idx = if n < 0.0 {
        items.len() as i64 + n as i64
    } else {
        n as i64
    };
    if idx < 0 || idx >= items.len() as i64 {
        return Ok(Value::Und);
    }
    Ok(items[idx as usize].clone())
}

fn arr_keys(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    if let Value::Obj(o) = this {
        return Ok(make_live_iter(o.clone(), i.p.array_iter.clone(), 1));
    }
    let items = to_elems(i, this)?;
    Ok(make_iter_in(
        (0..items.len()).map(|k| Value::Num(k as f64)).collect(),
        i.p.array_iter.clone(),
    ))
}

fn arr_values(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    if let Value::Obj(o) = this {
        return Ok(make_live_iter(o.clone(), i.p.array_iter.clone(), 0));
    }
    let items = to_elems(i, this)?;
    Ok(make_iter_in(items, i.p.array_iter.clone()))
}

fn arr_entries(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    if let Value::Obj(o) = this {
        return Ok(make_live_iter(o.clone(), i.p.array_iter.clone(), 2));
    }
    let items = to_elems(i, this)?;
    let mut out = Vec::new();
    for (k, v) in items.into_iter().enumerate() {
        out.push(Value::Obj(i.new_arr(vec![Value::Num(k as f64), v])));
    }
    Ok(make_iter_in(out, i.p.array_iter.clone()))
}

/// live-view iterator object — reads `obj` per step so mid-iteration
/// mutation (mapped arguments, array holes, map/set delete+add) shows
fn make_live_iter(o: ObjRef, proto: ObjRef, mode: u8) -> Value {
    Value::Obj(Rc::new(RefCell::new(Obj::new(
        Some(proto),
        ObjKind::IterLive { obj: o, idx: 0, mode },
    ))))
}

// ===========================================================================
// natives: string
// ===========================================================================

/// String receiver as `Rc<str>` — zero-copy for `Value::Str` (the common
/// case); callers slice/iterate instead of getting an owned `String`.
fn unbox_str(i: &mut Interp, this: &Value) -> Result<Rc<str>, Value> {
    match this {
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

/// Byte offset of char index `idx` in `s` (or `s.len()` when out of range).
fn char_byte_at(s: &str, idx: usize) -> usize {
    s.char_indices()
        .nth(idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

fn str_char_at(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let idx = i.to_num(&arg(args, 0)).unwrap_or(0.0) as usize;
    Ok(match s.chars().nth(idx) {
        Some(c) => char_val(c),
        None => Value::Str(empty_rc()),
    })
}

/// spec codePointAt: UTF-16 indexed, surrogate pairs combine
fn str_code_point_at(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let idx = i.to_num(&arg(args, 0)).unwrap_or(0.0);
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
                let cp = 0x10000
                    + ((first as u32 - 0xD800) << 10)
                    + (second as u32 - 0xDC00);
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
    let idx = i.to_num(&arg(args, 0)).unwrap_or(0.0) as usize;
    // UTF-16 code units — nth() instead of collecting a Vec<u16>
    Ok(Value::Num(
        s.encode_utf16()
            .nth(idx)
            .map(|u| u as f64)
            .unwrap_or(f64::NAN),
    ))
}

fn str_at(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let n = i.to_num(&arg(args, 0)).unwrap_or(0.0);
    let len = s.chars().count() as i64;
    let idx = if n < 0.0 { len + n as i64 } else { n as i64 };
    if idx < 0 || idx >= len {
        return Ok(Value::Und);
    }
    Ok(match s.chars().nth(idx as usize) {
        Some(c) => char_val(c),
        None => Value::Und,
    })
}

fn str_index_of(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let needle = to_rc(i, &arg(args, 0))?;
    let from = i.to_num(&arg(args, 1)).unwrap_or(0.0).max(0.0) as usize;
    let byte_from = char_byte_at(&s, from);
    match s[byte_from..].find(&*needle) {
        Some(b) => Ok(Value::Num(s[..byte_from + b].chars().count() as f64)),
        None => Ok(Value::Num(-1.0)),
    }
}

fn str_last_index_of(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let needle = i.to_str(&arg(args, 0))?;
    match s.rfind(&needle) {
        Some(b) => Ok(Value::Num(s[..b].chars().count() as f64)),
        None => Ok(Value::Num(-1.0)),
    }
}

fn str_slice(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let len = s.chars().count();
    let start = clamp_index(i.to_num(&arg(args, 0)).unwrap_or(0.0), len);
    let end = match arg(args, 1) {
        Value::Und => len,
        v => clamp_index(i.to_num(&v).unwrap_or(0.0), len),
    };
    // byte-range slice — no Vec<char> round-trip
    if start < end {
        let b0 = char_byte_at(&s, start);
        let b1 = char_byte_at(&s, end);
        Ok(Value::Str(Rc::from(&s[b0..b1])))
    } else {
        Ok(Value::Str(empty_rc()))
    }
}

fn str_substring(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let len = s.chars().count();
    let a = i.to_num(&arg(args, 0)).unwrap_or(0.0).max(0.0) as usize;
    let b = match arg(args, 1) {
        Value::Und => len,
        v => i.to_num(&v).unwrap_or(0.0).max(0.0) as usize,
    };
    let (lo, hi) = (a.min(b).min(len), a.max(b).min(len));
    let b0 = char_byte_at(&s, lo);
    let b1 = char_byte_at(&s, hi);
    Ok(Value::Str(Rc::from(&s[b0..b1])))
}

fn str_substr(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let len = s.chars().count() as f64;
    // ToIntegerOrInfinity — truncation, not clamp-then-round
    let mut start = i.to_num(&arg(args, 0))?.trunc();
    if start < 0.0 {
        start = (len + start).max(0.0);
    }
    let start = start.min(len);
    let count = match arg(args, 1) {
        Value::Und => len,
        v => i.to_num(&v)?.trunc().max(0.0),
    };
    let end = (start + count).min(len) as usize;
    let b0 = char_byte_at(&s, start as usize);
    let b1 = char_byte_at(&s, end);
    Ok(Value::Str(Rc::from(&s[b0..b1])))
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
    let t = s.to_lowercase();
    Ok(str_or_rc(s, &t))
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
        '\u{9}' | '\u{a}' | '\u{b}' | '\u{c}' | '\u{d}' | '\u{20}' | '\u{a0}'
            | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}'
            | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

fn str_trim(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    Ok(str_or_rc(
        s.clone(),
        s.trim_matches(is_js_space),
    ))
}

fn str_trim_start(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    Ok(str_or_rc(
        s.clone(),
        s.trim_start_matches(is_js_space),
    ))
}

fn str_trim_end(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    Ok(str_or_rc(
        s.clone(),
        s.trim_end_matches(is_js_space),
    ))
}

fn str_split(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let a0 = arg(args, 0);
    // Symbol.split protocol — GetMethod on any non-nullish separator
    if !matches!(a0, Value::Und | Value::Null) {
        if let Some(m) = get_method(i, &a0, &PropKey::Sym(SYM_SPLIT))? {
            let sv = Value::Str(s.clone());
            return i.call(&m, a0, vec![sv, arg(args, 1)]);
        }
    }
    let limit = match arg(args, 1) {
        Value::Und => usize::MAX,
        v => to_uint32_num(i.to_num(&v)?) as usize,
    };
    // spec: lim = 0 short-circuits before ToString(separator)
    if limit == 0 {
        return Ok(Value::Obj(i.new_arr(Vec::new())));
    }
    let parts: Vec<Value> = match a0 {
        Value::Und => vec![Value::Str(s)],
        v => {
            let sep = re_coerce_str(i, &v)?;
            if sep.is_empty() {
                s.chars().take(limit).map(char_val).collect()
            } else {
                s.split(sep.as_str())
                    .take(limit)
                    .map(|p| Value::Str(Rc::from(p)))
                    .collect()
            }
        }
    };
    Ok(Value::Obj(i.new_arr(parts)))
}

fn str_replace(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let a0 = arg(args, 0);
    // Symbol.replace protocol — GetMethod on any non-nullish searchValue
    if !matches!(a0, Value::Und | Value::Null) {
        if let Some(m) = get_method(i, &a0, &PropKey::Sym(SYM_REPLACE))? {
            let sv = Value::Str(s.clone());
            return i.call(&m, a0, vec![sv, arg(args, 1)]);
        }
    }
    let needle = re_coerce_str(i, &a0)?;
    let repl_fn = arg(args, 1);
    match s.find(&needle) {
        Some(b) => {
            let matched = needle.clone();
            let rep = if i.is_callable(&repl_fn) {
                let r = i.call(
                    &repl_fn,
                    Value::Und,
                    vec![
                        Value::Str(Rc::from(matched.as_str())),
                        Value::Num(s[..b].chars().count() as f64),
                        Value::Str(s.clone()),
                    ],
                )?;
                re_coerce_str(i, &r)?
            } else {
                expand_replacement(&re_coerce_str(i, &repl_fn)?, &matched, b, &s)
            };
            let mut out = s[..b].to_string();
            out.push_str(&rep);
            out.push_str(&s[b + needle.len()..]);
            Ok(Value::Str(Rc::from(out.as_str())))
        }
        None => Ok(Value::Str(s)),
    }
}

fn str_replace_all(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let a0 = arg(args, 0);
    if !matches!(a0, Value::Und | Value::Null) {
        // spec: IsRegExp(regexp) → ToString(Get(regexp,"flags")) must have 'g'
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
            let sv = Value::Str(s.clone());
            return i.call(&m, a0, vec![sv, arg(args, 1)]);
        }
    }
    let needle = re_coerce_str(i, &a0)?;
    let repl_fn = arg(args, 1);
    let mut out = String::new();
    let mut pos = 0usize;
    if needle.is_empty() {
        // spec: empty search inserts replacement at every char boundary
        let callable = i.is_callable(&repl_fn);
        let rep_str = if !callable {
            re_coerce_str(i, &repl_fn)?
        } else {
            String::new()
        };
        for (b, _) in s.char_indices().chain(std::iter::once((s.len(), '\0'))) {
            out.push_str(&s[pos..b]);
            if callable {
                let r = i.call(
                    &repl_fn,
                    Value::Und,
                    vec![
                        Value::Str(Rc::from("")),
                        Value::Num(s[..b].chars().count() as f64),
                        Value::Str(s.clone()),
                    ],
                )?;
                out.push_str(&re_coerce_str(i, &r)?);
            } else {
                out.push_str(&expand_replacement(&rep_str, "", b, &s));
            }
            pos = b;
        }
        out.push_str(&s[pos..]);
        return Ok(Value::Str(Rc::from(out.as_str())));
    }
    for (b, _) in s.match_indices(&needle) {
        out.push_str(&s[pos..b]);
        if i.is_callable(&repl_fn) {
            let r = i.call(
                &repl_fn,
                Value::Und,
                vec![
                    Value::Str(Rc::from(needle.as_str())),
                    Value::Num(s[..b].chars().count() as f64),
                    Value::Str(s.clone()),
                ],
            )?;
            out.push_str(&re_coerce_str(i, &r)?);
        } else {
            let rep = re_coerce_str(i, &repl_fn)?;
            out.push_str(&expand_replacement(&rep, &needle, b, &s));
        }
        pos = b + needle.len();
    }
    out.push_str(&s[pos..]);
    Ok(Value::Str(Rc::from(out.as_str())))
}

fn str_match(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let a0 = arg(args, 0);
    if !matches!(a0, Value::Und | Value::Null) {
        if let Some(m) = get_method(i, &a0, &PropKey::Sym(SYM_MATCH))? {
            let sv = Value::Str(unbox_str(i, this)?);
            return i.call(&m, a0, vec![sv]);
        }
    }
    // non-regex arg → RegExpCreate(arg) then Invoke @@match
    let sv = Value::Str(unbox_str(i, this)?);
    let rx = regexp_create(i, &a0, &Value::Und)?;
    let m = i.get_prop(&rx, &PropKey::Sym(SYM_MATCH))?;
    i.call(&m, rx, vec![sv])
}

fn str_match_all(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let a0 = arg(args, 0);
    if !matches!(a0, Value::Und | Value::Null) {
        // spec: IsRegExp(regexp) → ToString(Get(regexp,"flags")) must have 'g'
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
            let sv = Value::Str(unbox_str(i, this)?);
            return i.call(&m, a0, vec![sv]);
        }
    }
    // fallback: RegExpCreate(arg, "g") then Invoke @@matchAll
    let sv = Value::Str(unbox_str(i, this)?);
    let rx = regexp_create(i, &a0, &Value::Str(Rc::from("g")))?;
    let m = i.get_prop(&rx, &PropKey::Sym(SYM_MATCHALL))?;
    i.call(&m, rx, vec![sv])
}

fn str_search(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let a0 = arg(args, 0);
    if !matches!(a0, Value::Und | Value::Null) {
        if let Some(m) = get_method(i, &a0, &PropKey::Sym(SYM_SEARCH))? {
            let sv = Value::Str(unbox_str(i, this)?);
            return i.call(&m, a0, vec![sv]);
        }
    }
    let sv = Value::Str(unbox_str(i, this)?);
    let rx = regexp_create(i, &a0, &Value::Und)?;
    let m = i.get_prop(&rx, &PropKey::Sym(SYM_SEARCH))?;
    i.call(&m, rx, vec![sv])
}

fn expand_replacement(rep: &str, matched: &str, pos: usize, whole: &str) -> String {
    let mut out = String::new();
    let mut chars = rep.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' {
            match chars.peek() {
                Some('$') => {
                    chars.next();
                    out.push('$');
                }
                Some('&') => {
                    chars.next();
                    out.push_str(matched);
                }
                Some('`') => {
                    chars.next();
                    out.push_str(&whole[..pos]);
                }
                Some('\'') => {
                    chars.next();
                    out.push_str(&whole[pos + matched.len()..]);
                }
                _ => out.push('$'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn str_pad_start(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let target = i.to_num(&arg(args, 0)).unwrap_or(0.0) as usize;
    if target > 1_000_000 {
        return Err(i.range_err("Invalid string length"));
    }
    let pad = match arg(args, 1) {
        Value::Und => " ".to_string(),
        v => i.to_str(&v)?,
    };
    let cur = s.chars().count();
    if cur >= target || pad.is_empty() {
        return Ok(Value::Str(s));
    }
    let need = target - cur;
    let mut p = String::new();
    while p.chars().count() < need {
        p.push_str(&pad);
    }
    let p: String = p.chars().take(need).collect();
    Ok(Value::Str(Rc::from(format!("{p}{s}").as_str())))
}

fn str_pad_end(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let target = i.to_num(&arg(args, 0)).unwrap_or(0.0) as usize;
    if target > 1_000_000 {
        return Err(i.range_err("Invalid string length"));
    }
    let pad = match arg(args, 1) {
        Value::Und => " ".to_string(),
        v => i.to_str(&v)?,
    };
    let cur = s.chars().count();
    if cur >= target || pad.is_empty() {
        return Ok(Value::Str(s));
    }
    let need = target - cur;
    let mut p = String::new();
    while p.chars().count() < need {
        p.push_str(&pad);
    }
    let p: String = p.chars().take(need).collect();
    Ok(Value::Str(Rc::from(format!("{s}{p}").as_str())))
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
    let needle = i.to_str(&arg(args, 0))?;
    let pos = i.to_num(&arg(args, 1)).unwrap_or(0.0).max(0.0) as usize;
    let byte = s.char_indices().nth(pos).map(|(b, _)| b).unwrap_or(s.len());
    Ok(Value::Bool(s[byte..].starts_with(&needle)))
}

fn str_ends_with(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    reject_regex_arg(i, &arg(args, 0), "endsWith")?;
    let needle = i.to_str(&arg(args, 0))?;
    // spec: position is the END index (default = string length)
    let chars = s.chars().count();
    let pos = match arg(args, 1) {
        Value::Und => chars,
        v => (i.to_num(&v)?.max(0.0) as usize).min(chars),
    };
    let byte = s
        .char_indices()
        .nth(pos)
        .map(|(b, _)| b)
        .unwrap_or(s.len());
    Ok(Value::Bool(s[..byte].ends_with(&needle)))
}

fn str_includes(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    reject_regex_arg(i, &arg(args, 0), "includes")?;
    let needle = i.to_str(&arg(args, 0))?;
    let pos = i.to_num(&arg(args, 1)).unwrap_or(0.0).max(0.0) as usize;
    let byte = s.char_indices().nth(pos).map(|(b, _)| b).unwrap_or(s.len());
    Ok(Value::Bool(s[byte..].contains(&needle)))
}

fn str_repeat(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let n = i.to_num(&arg(args, 0))?.max(0.0) as usize;
    if n > 1_000_000 {
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
        return Err(i.type_err(
            "String.prototype method called on null or undefined",
        ));
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
    Ok(Value::Str(unbox_str(i, this)?))
}

fn str_locale_compare(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let o = i.to_str(&arg(args, 0))?;
    Ok(Value::Num(match (&*s).cmp(o.as_str()) {
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

fn string_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = if args.is_empty() {
        String::new()
    } else {
        // explicit String(symbol) is legal — only implicit ToString throws
        match &arg(args, 0) {
            Value::Sym(_, d) => format!("Symbol({d})"),
            v => i.to_str(v)?,
        }
    };
    if matches!(this, Value::Obj(_)) {
        // `new String(x)` → boxed — string-exotic own props: chars are
        // {W:false,E:true,C:false}, length {W:false,E:false,C:false}
        if let Value::Obj(o) = this {
            i.set_builtin(o, "$value", Value::Str(Rc::from(s.as_str())));
            let len = s.chars().count();
            let mut ob = o.borrow_mut();
            for (k, c) in s.chars().enumerate() {
                ob.props.insert(
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
            drop(ob);
            i.set_attr(o, "length", Value::Num(len as f64), false, false);
        }
        Ok(this.clone())
    } else {
        Ok(Value::Str(Rc::from(s.as_str())))
    }
}

fn string_from_char_code(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let mut s = String::new();
    for a in args {
        let n = i.to_num(a)? as u32;
        if let Some(c) = char::from_u32(n) {
            s.push(c);
        }
    }
    Ok(Value::Str(Rc::from(s.as_str())))
}

fn string_raw(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let strings = arg(args, 0);
    let len_v = i.get_prop_str(&strings, "length")?;
    let n = i.to_num(&len_v)? as usize;
    let mut s = String::new();
    for k in 0..n {
        let seg = i.get_prop(&strings, &idx_key(k))?;
        s.push_str(&i.to_str(&seg)?);
        if k + 1 < n {
            if let Some(sub) = args.get(k + 1) {
                s.push_str(&i.to_str(sub)?);
            }
        }
    }
    Ok(Value::Str(Rc::from(s.as_str())))
}

// ===========================================================================
// natives: number / boolean / symbol
// ===========================================================================

fn unbox_num(i: &mut Interp, this: &Value) -> Result<f64, Value> {
    match this {
        Value::Num(n) => Ok(*n),
        Value::Obj(o) => {
            if let Some(v) = i.obj_get_raw_str(o, "$value") {
                return i.to_num(&v);
            }
            Err(i.type_err("Number.prototype method called on incompatible receiver"))
        }
        other => i.to_num(other),
    }
}

fn num_to_fixed(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let n = unbox_num(i, this)?;
    let d = i.to_num(&arg(args, 0)).unwrap_or(0.0).clamp(0.0, 100.0) as usize;
    if !n.is_finite() {
        return Ok(Value::Str(Rc::from(js_num_str(n).as_str())));
    }
    Ok(Value::Str(Rc::from(format!("{n:.d$}").as_str())))
}

fn num_to_precision(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let n = unbox_num(i, this)?;
    match arg(args, 0) {
        Value::Und => Ok(Value::Str(Rc::from(js_num_str(n).as_str()))),
        v => {
            let d = i.to_num(&v)?.clamp(1.0, 100.0) as usize;
            if n == 0.0 {
                return Ok(Value::Str(Rc::from("0".repeat(d).as_str())));
            }
            let mag = n.abs().log10().floor() as i32;
            let decimals = (d as i32 - 1 - mag).max(0) as usize;
            Ok(Value::Str(Rc::from(format!("{n:.decimals$}").as_str())))
        }
    }
}

fn num_to_exponential(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let n = unbox_num(i, this)?;
    let d = i.to_num(&arg(args, 0)).unwrap_or(6.0).clamp(0.0, 100.0) as usize;
    if !n.is_finite() {
        return Ok(Value::Str(Rc::from(js_num_str(n).as_str())));
    }
    let s = format!("{n:.d$e}");
    // rust gives "1.5e2" — JS wants "1.5e+2"
    let s = s
        .replace("e-", "e-")
        .replacen('e', "e+", 1)
        .replace("e+-", "e-");
    Ok(Value::Str(Rc::from(s.as_str())))
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
    let n = match arg(args, 0) {
        Value::Und => 0.0,
        v => i.to_num(&v)?,
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
    match i.run(&src) {
        Ok(v) => Ok(v),
        Err(e) => Err(i.syntax_err(format!("{e}"))),
    }
}

fn symbol_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    if matches!(this, Value::Obj(_)) {
        return Err(i.type_err("Symbol is not a constructor"));
    }
    let d = match arg(args, 0) {
        Value::Und => String::new(),
        v => i.to_str(&v)?,
    };
    let id = i.sym_counter;
    i.sym_counter += 1;
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
    match arg(args, 0) {
        // only Symbol.for-registry ids (>= 1_000_000) have a key
        Value::Sym(id, d) if id >= 1_000_000 => {
            Ok(Value::Str(Rc::from(d.as_ref())))
        }
        _ => Ok(Value::Und),
    }
}

fn sym_to_string(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let _ = i;
    match this {
        Value::Sym(_, d) => Ok(Value::Str(Rc::from(format!("Symbol({d})").as_str()))),
        _ => Ok(Value::Str(Rc::from("Symbol()"))),
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
    let _ = i;
    match this {
        Value::Sym(_, d) => {
            if d.is_empty() {
                Ok(Value::Und)
            } else {
                Ok(Value::Str(Rc::from(d.as_ref())))
            }
        }
        _ => Ok(Value::Und),
    }
}

// ===========================================================================
// natives: errors
// ===========================================================================

fn error_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let msg = match arg(args, 0) {
        Value::Und => String::new(),
        v => i.to_str(&v)?,
    };
    if let Value::Obj(o) = this {
        o.borrow_mut().kind = ObjKind::Err;
        i.set_attr(o, "message", Value::Str(Rc::from(msg.as_str())), true, true);
        i.set_attr(o, "name", Value::Str(Rc::from("Error")), true, true);
        i.set_attr(
            o,
            "stack",
            Value::Str(Rc::from(format!("Error: {msg}").as_str())),
            true,
            true,
        );
        return Ok(this.clone());
    }
    let p = i.p.error.clone();
    Ok(i.err_obj(&p, "Error", &msg))
}

macro_rules! sub_err_ctor {
    ($name:ident, $field:ident, $jname:literal) => {
        fn $name(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
            let msg = match arg(args, 0) {
                Value::Und => String::new(),
                v => i.to_str(&v)?,
            };
            if let Value::Obj(o) = this {
                o.borrow_mut().kind = ObjKind::Err;
                i.set_attr(o, "message", Value::Str(Rc::from(msg.as_str())), true, true);
                i.set_attr(o, "name", Value::Str(Rc::from($jname)), true, true);
                i.set_attr(
                    o,
                    "stack",
                    Value::Str(Rc::from(format!("{}: {}", $jname, msg).as_str())),
                    true,
                    true,
                );
                return Ok(this.clone());
            }
            let p = i.p.$field.clone();
            Ok(i.err_obj(&p, $jname, &msg))
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
    if let Value::Obj(o) = this {
        let name = i
            .obj_get_raw_str(o, "name")
            .map(|v| i.to_str(&v).unwrap_or_else(|_| "Error".into()))
            .unwrap_or_else(|| "Error".into());
        let msg = i
            .obj_get_raw_str(o, "message")
            .map(|v| i.to_str(&v).unwrap_or_default())
            .unwrap_or_default();
        if msg.is_empty() {
            return Ok(Value::Str(Rc::from(name.as_str())));
        }
        return Ok(Value::Str(Rc::from(format!("{name}: {msg}").as_str())));
    }
    Ok(Value::Str(Rc::from("Error")))
}


/// `AggregateError(iterable, message)` — collects the iterable into an
/// `errors` own data property
fn agg_error_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let errs = match args.first() {
        Some(v) => i.iter_values(v)?,
        None => Vec::new(),
    };
    let msg = match args.get(1).cloned().unwrap_or(Value::Und) {
        Value::Und => String::new(),
        v => i.to_str(&v)?,
    };
    let arr = Value::Obj(i.new_arr(errs));
    if let Value::Obj(o) = this {
        o.borrow_mut().kind = ObjKind::Err;
        i.set_attr(o, "errors", arr, true, true);
        i.set_attr(o, "message", Value::Str(Rc::from(msg.as_str())), true, true);
        i.set_attr(o, "name", Value::Str(Rc::from("AggregateError")), true, true);
        return Ok(this.clone());
    }
    let e = i.err_obj(&i.p.agg_error.clone(), "AggregateError", &msg);
    if let Value::Obj(o) = &e {
        i.set_attr(o, "errors", arr, true, true);
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
        i.set_attr(o, "message", Value::Str(Rc::from(msg.as_str())), true, true);
        i.set_attr(o, "name", Value::Str(Rc::from("SuppressedError")), true, true);
        return Ok(this.clone());
    }
    let e = i.err_obj(&i.p.suppressed_error.clone(), "SuppressedError", &msg);
    if let Value::Obj(o) = &e {
        i.set_attr(o, "error", err, true, true);
        i.set_attr(o, "suppressed", supp, true, true);
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
    // JS rounds half toward +inf
    (n + 0.5).floor()
});
math1!(math_trunc, f64::trunc);
math1!(math_abs, f64::abs);
math1!(math_sign, |n: f64| if n.is_nan() {
    f64::NAN
} else {
    n.signum()
});
math1!(math_sqrt, f64::sqrt);
math1!(math_cbrt, f64::cbrt);
math1!(math_log, f64::ln);
math1!(math_log2, f64::log2);
math1!(math_log10, f64::log10);
math1!(math_exp, f64::exp);
math1!(math_sin, f64::sin);
math1!(math_cos, f64::cos);
math1!(math_tan, f64::tan);
math1!(math_asin, f64::asin);
math1!(math_acos, f64::acos);
math1!(math_atan, f64::atan);

fn math_min(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let mut m = f64::INFINITY;
    for a in args {
        let n = i.to_num(a)?;
        if n.is_nan() {
            return Ok(Value::Num(f64::NAN));
        }
        m = m.min(n);
    }
    Ok(Value::Num(m))
}

fn math_max(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let mut m = f64::NEG_INFINITY;
    for a in args {
        let n = i.to_num(a)?;
        if n.is_nan() {
            return Ok(Value::Num(f64::NAN));
        }
        m = m.max(n);
    }
    Ok(Value::Num(m))
}

fn math_pow(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    Ok(Value::Num(
        i.to_num(&arg(args, 0))?.powf(i.to_num(&arg(args, 1))?),
    ))
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
    let mut s = 0.0;
    for a in args {
        let n = i.to_num(a)?;
        s += n * n;
    }
    Ok(Value::Num(s.sqrt()))
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
    match p.value(i) {
        Ok(v) => {
            p.skip_ws();
            if p.pos != p.chars.len() {
                return Err(i.syntax_err("Unexpected token in JSON"));
            }
            Ok(v)
        }
        Err(msg) => Err(i.syntax_err(format!("Unexpected token in JSON: {msg}"))),
    }
}

struct JsonParser {
    chars: Vec<char>,
    pos: usize,
}

impl JsonParser {
    fn skip_ws(&mut self) {
        while self.pos < self.chars.len() && self.chars[self.pos].is_whitespace() {
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
            Some(c) if c == '-' || c.is_ascii_digit() => {
                let start = self.pos;
                while self
                    .peek()
                    .is_some_and(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
                {
                    self.pos += 1;
                }
                let s: String = self.chars[start..self.pos].iter().collect();
                s.parse::<f64>()
                    .map(Value::Num)
                    .map_err(|_| format!("bad number {s}"))
            }
            _ => Err("unexpected char".into()),
        }
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
    let indent = match arg(args, 2) {
        Value::Num(n) => " ".repeat((n as usize).min(10)),
        Value::Str(s) => s.chars().take(10).collect(),
        _ => String::new(),
    };
    let mut out = String::new();
    let mut stack = Vec::new();
    if json_ser_into(i, &v, &indent, 0, &mut out, &mut stack)? {
        Ok(Value::Str(Rc::from(out.as_str())))
    } else {
        Ok(Value::Und)
    }
}

fn json_pad(out: &mut String, indent: &str, level: usize) {
    for _ in 0..level {
        out.push_str(indent);
    }
}

/// Serialize into `out` — returns false when the value is omitted
/// (undefined/symbol/function). Writes directly instead of building a
/// `String` per node plus a `Vec<String>` join per container.
fn json_ser_into(
    i: &mut Interp,
    v: &Value,
    indent: &str,
    level: usize,
    out: &mut String,
    stack: &mut Vec<usize>,
) -> Result<bool, Value> {
    // replacer via toJSON
    let tj = i.get_prop_str(v, "toJSON").unwrap_or(Value::Und);
    let v2 = if i.is_callable(&tj) {
        i.call(&tj, v.clone(), vec![Value::Str(empty_rc())])?
    } else {
        v.clone()
    };
    Ok(match &v2 {
        Value::Und | Value::Sym(..) => false,
        Value::Null => {
            out.push_str("null");
            true
        }
        Value::Bool(b) => {
            out.push_str(if *b { "true" } else { "false" });
            true
        }
        Value::Num(n) => {
            if n.is_finite() {
                out.push_str(&js_num_str(*n));
            } else {
                out.push_str("null");
            }
            true
        }
        Value::Str(s) => {
            json_escape_into(s, out);
            true
        }
        Value::Obj(o) => {
            // cyclic structure → TypeError (JSON.stringify spec); the
            // ancestor stack distinguishes cycles from merely shared refs
            let ptr = Rc::as_ptr(o) as usize;
            if stack.contains(&ptr) {
                return Err(i.type_err("circular structure"));
            }
            stack.push(ptr);
            enum K {
                Arr(Vec<Value>),
                Obj,
                Skip,
            }
            // short borrow only — recursion can run getters that mutate this
            let k = {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Arr(items) => K::Arr(items.clone()),
                    ObjKind::Func(_) | ObjKind::Bound { .. } => K::Skip,
                    _ => K::Obj,
                }
            };
            let r: Result<bool, Value> = (|| {
                Ok(match k {
                    K::Skip => false,
                    K::Arr(items) => {
                        if indent.is_empty() {
                            out.push('[');
                            for (n, it) in items.iter().enumerate() {
                                if n > 0 {
                                    out.push(',');
                                }
                                if !json_ser_into(i, it, indent, level + 1, out, stack)? {
                                    out.push_str("null");
                                }
                            }
                            out.push(']');
                        } else if items.is_empty() {
                            out.push_str("[]");
                        } else {
                            out.push_str("[\n");
                            json_pad(out, indent, level + 1);
                            for (n, it) in items.iter().enumerate() {
                                if n > 0 {
                                    out.push_str(",\n");
                                    json_pad(out, indent, level + 1);
                                }
                                if !json_ser_into(i, it, indent, level + 1, out, stack)? {
                                    out.push_str("null");
                                }
                            }
                            out.push('\n');
                            json_pad(out, indent, level);
                            out.push(']');
                        }
                        true
                    }
                    K::Obj => {
                        let entries = own_enum_entries(i, &v2);
                        let sep_colon: &str = if indent.is_empty() { ":" } else { ": " };
                        let open_len = out.len();
                        if indent.is_empty() {
                            out.push('{');
                        } else {
                            out.push_str("{\n");
                            json_pad(out, indent, level + 1);
                        }
                        let mut first = true;
                        for (key, slot) in entries {
                            let val = match slot {
                                Some(val) => val,
                                None => i.get_prop(&v2, &key)?,
                            };
                            // serialize first — skip the key entirely when the
                            // value is omitted
                            let mut tmp = String::new();
                            if json_ser_into(i, &val, indent, level + 1, &mut tmp, stack)? {
                                if !first {
                                    if indent.is_empty() {
                                        out.push(',');
                                    } else {
                                        out.push_str(",\n");
                                        json_pad(out, indent, level + 1);
                                    }
                                }
                                first = false;
                                if let PropKey::S(s) = &key {
                                    json_escape_into(s, out);
                                }
                                out.push_str(sep_colon);
                                out.push_str(&tmp);
                            }
                        }
                        if indent.is_empty() {
                            out.push('}');
                        } else if first {
                            // nothing written (empty obj or all values omitted) → {}
                            out.truncate(open_len);
                            out.push_str("{}");
                        } else {
                            out.push('\n');
                            json_pad(out, indent, level);
                            out.push('}');
                        }
                        true
                    }
                })
            })();
            stack.pop();
            r?
        }
    })
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
// natives: promise (stub — synchronous)
// ===========================================================================

fn new_promise(i: &mut Interp) -> ObjRef {
    Rc::new(RefCell::new(Obj::new(
        Some(i.p.promise.clone()),
        ObjKind::Promise(PromiseState::Pending),
    )))
}

fn promise_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let p = if let Value::Obj(o) = this {
        o.borrow_mut().kind = ObjKind::Promise(PromiseState::Pending);
        o.clone()
    } else {
        new_promise(i)
    };
    // handlers queue on a prop
    i.set_builtin(&p, "$handlers", Value::Obj(i.new_arr(Vec::new())));
    let executor = arg(args, 0);
    if i.is_callable(&executor) {
        let pv = Value::Obj(p.clone());
        let res_fn = i.native("resolve", promise_resolve_inner);
        let rej_fn = i.native("reject", promise_reject_inner);
        let res = Rc::new(RefCell::new(Obj::new(
            Some(i.p.func.clone()),
            ObjKind::Bound {
                target: res_fn,
                thisv: pv.clone(),
                extra: Vec::new(),
            },
        )));
        let rej = Rc::new(RefCell::new(Obj::new(
            Some(i.p.func.clone()),
            ObjKind::Bound {
                target: rej_fn,
                thisv: pv,
                extra: Vec::new(),
            },
        )));
        let r = i.call(
            &executor,
            Value::Und,
            vec![Value::Obj(res), Value::Obj(rej)],
        );
        if let Err(e) = r {
            settle(i, &p, false, e)?;
        }
    }
    Ok(Value::Obj(p))
}

/// resolve/reject implementation; `this` = the promise object.
fn promise_resolve_inner(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    if let Value::Obj(p) = this {
        // resolving with a promise adopts its state
        if let Value::Obj(po) = &arg(args, 0) {
            if let ObjKind::Promise(_) = po.borrow().kind {
                let st = po.borrow().kind.clone();
                match st {
                    ObjKind::Promise(PromiseState::Fulfilled(v)) => {
                        return settle(i, p, true, *v);
                    }
                    ObjKind::Promise(PromiseState::Rejected(v)) => {
                        return settle(i, p, false, *v);
                    }
                    ObjKind::Promise(PromiseState::Pending) => {
                        // forward via then
                        let then = i.get_prop_str(&arg(args, 0), "then")?;
                        let pv = Value::Obj(p.clone());
                        let res_fn = i.native("resolve", promise_resolve_inner);
                        let rej_fn = i.native("reject", promise_reject_inner);
                        let res = Rc::new(RefCell::new(Obj::new(
                            Some(i.p.func.clone()),
                            ObjKind::Bound {
                                target: res_fn,
                                thisv: pv.clone(),
                                extra: vec![],
                            },
                        )));
                        let rej = Rc::new(RefCell::new(Obj::new(
                            Some(i.p.func.clone()),
                            ObjKind::Bound {
                                target: rej_fn,
                                thisv: pv,
                                extra: vec![],
                            },
                        )));
                        return i.call(&then, arg(args, 0), vec![Value::Obj(res), Value::Obj(rej)]);
                    }
                    _ => {}
                }
            }
        }
        settle(i, p, true, arg(args, 0))
    } else {
        Ok(Value::Und)
    }
}

fn promise_reject_inner(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    if let Value::Obj(p) = this {
        settle(i, p, false, arg(args, 0))
    } else {
        Ok(Value::Und)
    }
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
    run_handlers(i, p)
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

fn run_handlers(i: &mut Interp, p: &ObjRef) -> NResult {
    let st = match &p.borrow().kind {
        ObjKind::Promise(s) => s.clone(),
        _ => return Ok(Value::Und),
    };
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
        match &st {
            PromiseState::Fulfilled(v) => {
                let v = (**v).clone();
                if i.is_callable(&on_f) {
                    match i.call(&on_f, Value::Und, vec![v]) {
                        Ok(r) => {
                            i.call(&resolve, Value::Und, vec![r])?;
                        }
                        Err(e) => {
                            i.call(&reject, Value::Und, vec![e])?;
                        }
                    }
                } else {
                    i.call(&resolve, Value::Und, vec![v])?;
                }
            }
            PromiseState::Rejected(v) => {
                let v = (**v).clone();
                if i.is_callable(&on_r) {
                    match i.call(&on_r, Value::Und, vec![v.clone()]) {
                        Ok(r) => {
                            i.call(&resolve, Value::Und, vec![r])?;
                        }
                        Err(e) => {
                            i.call(&reject, Value::Und, vec![e])?;
                        }
                    }
                } else {
                    i.call(&reject, Value::Und, vec![v])?;
                }
            }
            PromiseState::Pending => {}
        }
    }
    // clear handlers
    if let Some(Value::Obj(h)) = p
        .borrow()
        .props
        .get_str("$handlers")
        .map(|pr| pr.val.clone())
    {
        h.borrow_mut().kind = ObjKind::Arr(Vec::new());
    }
    Ok(Value::Und)
}

fn promise_then(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let Value::Obj(p) = this else {
        return Err(i.type_err("Promise.prototype.then called on non-promise"));
    };
    let p2 = new_promise(i);
    i.set_builtin(&p2, "$handlers", Value::Obj(i.new_arr(Vec::new())));
    // build handler record
    let rec = i.new_obj();
    i.set_builtin(&rec, "onF", arg(args, 0));
    i.set_builtin(&rec, "onR", arg(args, 1));
    // resolver fns bound to p2
    let pv = Value::Obj(p2.clone());
    let res_fn = i.native("resolve", promise_resolve_inner);
    let rej_fn = i.native("reject", promise_reject_inner);
    let res = Rc::new(RefCell::new(Obj::new(
        Some(i.p.func.clone()),
        ObjKind::Bound {
            target: res_fn,
            thisv: pv.clone(),
            extra: vec![],
        },
    )));
    let rej = Rc::new(RefCell::new(Obj::new(
        Some(i.p.func.clone()),
        ObjKind::Bound {
            target: rej_fn,
            thisv: pv,
            extra: vec![],
        },
    )));
    i.set_builtin(&rec, "resolve", Value::Obj(res));
    i.set_builtin(&rec, "reject", Value::Obj(rej));
    // enqueue
    if let Some(Value::Obj(h)) = p
        .borrow()
        .props
        .get_str("$handlers")
        .map(|pr| pr.val.clone())
    {
        if let ObjKind::Arr(items) = &mut h.borrow_mut().kind {
            items.push(Value::Obj(rec));
        }
    }
    run_handlers(i, p)?;
    Ok(Value::Obj(p2))
}

fn promise_catch(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    promise_then(i, this, &[Value::Und, arg(args, 0)])
}

fn promise_finally(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let Value::Obj(p) = this else {
        return Err(i.type_err("finally called on non-promise"));
    };
    let p2 = new_promise(i);
    i.set_builtin(&p2, "$handlers", Value::Obj(i.new_arr(Vec::new())));
    let st = match &p.borrow().kind {
        ObjKind::Promise(s) => s.clone(),
        _ => PromiseState::Pending,
    };
    match st {
        PromiseState::Fulfilled(v) => {
            if i.is_callable(&cb) {
                i.call(&cb, Value::Und, vec![])?;
            }
            p2.borrow_mut().kind = ObjKind::Promise(PromiseState::Fulfilled(v));
        }
        PromiseState::Rejected(v) => {
            if i.is_callable(&cb) {
                i.call(&cb, Value::Und, vec![])?;
            }
            p2.borrow_mut().kind = ObjKind::Promise(PromiseState::Rejected(v));
        }
        PromiseState::Pending => {
            // attach record that will run cb then forward
            let rec = i.new_obj();
            // wrapping: onF runs cb then resolves with value — approximate via handler passthrough
            i.set_builtin(&rec, "onF", cb.clone());
            i.set_builtin(&rec, "onR", cb);
            let pv = Value::Obj(p2.clone());
            let res_fn = i.native("resolve", promise_resolve_inner);
            let rej_fn = i.native("reject", promise_reject_inner);
            let res = Rc::new(RefCell::new(Obj::new(
                Some(i.p.func.clone()),
                ObjKind::Bound {
                    target: res_fn,
                    thisv: pv.clone(),
                    extra: vec![],
                },
            )));
            let rej = Rc::new(RefCell::new(Obj::new(
                Some(i.p.func.clone()),
                ObjKind::Bound {
                    target: rej_fn,
                    thisv: pv,
                    extra: vec![],
                },
            )));
            i.set_builtin(&rec, "resolve", Value::Obj(res));
            i.set_builtin(&rec, "reject", Value::Obj(rej));
            if let Some(Value::Obj(h)) = p
                .borrow()
                .props
                .get_str("$handlers")
                .map(|pr| pr.val.clone())
            {
                if let ObjKind::Arr(items) = &mut h.borrow_mut().kind {
                    items.push(Value::Obj(rec));
                }
            }
        }
    }
    Ok(Value::Obj(p2))
}

pub(crate) fn promise_resolve_static(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let v = arg(args, 0);
    if let Value::Obj(o) = &v {
        if matches!(o.borrow().kind, ObjKind::Promise(_)) {
            return Ok(v);
        }
    }
    let p = new_promise(i);
    i.set_builtin(&p, "$handlers", Value::Obj(i.new_arr(Vec::new())));
    p.borrow_mut().kind = ObjKind::Promise(PromiseState::Fulfilled(Box::new(v)));
    Ok(Value::Obj(p))
}

pub(crate) fn promise_reject_static(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let p = new_promise(i);
    i.set_builtin(&p, "$handlers", Value::Obj(i.new_arr(Vec::new())));
    p.borrow_mut().kind = ObjKind::Promise(PromiseState::Rejected(Box::new(arg(args, 0))));
    Ok(Value::Obj(p))
}

fn promise_all(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let items = i.iter_values(&arg(args, 0))?;
    let mut out = Vec::new();
    for it in items {
        if let Value::Obj(o) = &it {
            match &o.borrow().kind {
                ObjKind::Promise(PromiseState::Fulfilled(v)) => out.push((**v).clone()),
                ObjKind::Promise(PromiseState::Rejected(v)) => {
                    let p = new_promise(i);
                    i.set_builtin(&p, "$handlers", Value::Obj(i.new_arr(Vec::new())));
                    p.borrow_mut().kind = ObjKind::Promise(PromiseState::Rejected(v.clone()));
                    return Ok(Value::Obj(p));
                }
                ObjKind::Promise(PromiseState::Pending) => {
                    // leave result pending forever (stub)
                    let p = new_promise(i);
                    i.set_builtin(&p, "$handlers", Value::Obj(i.new_arr(Vec::new())));
                    return Ok(Value::Obj(p));
                }
                _ => out.push(it.clone()),
            }
        } else {
            out.push(it);
        }
    }
    let p = new_promise(i);
    i.set_builtin(&p, "$handlers", Value::Obj(i.new_arr(Vec::new())));
    let arr = Value::Obj(i.new_arr(out));
    p.borrow_mut().kind = ObjKind::Promise(PromiseState::Fulfilled(Box::new(arr)));
    Ok(Value::Obj(p))
}

fn promise_race(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let items = i.iter_values(&arg(args, 0))?;
    for it in items {
        if let Value::Obj(o) = &it {
            match &o.borrow().kind {
                ObjKind::Promise(st) => {
                    let p = new_promise(i);
                    i.set_builtin(&p, "$handlers", Value::Obj(i.new_arr(Vec::new())));
                    p.borrow_mut().kind = ObjKind::Promise(st.clone());
                    return Ok(Value::Obj(p));
                }
                _ => {}
            }
        }
    }
    let p = new_promise(i);
    i.set_builtin(&p, "$handlers", Value::Obj(i.new_arr(Vec::new())));
    Ok(Value::Obj(p))
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
    let mut given: Vec<f64> = Vec::with_capacity(args.len().min(slots.len()));
    for a in args.iter().take(slots.len()) {
        given.push(i.to_num(a)?);
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
        _ => {
            return Err(i.type_err("invalid hint"))
        }
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
            Some(s) if s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit()) => {
                s.parse().ok()
            }
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
    let ok_len = if signed { ystr.len() == 6 } else { ystr.len() == 4 };
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
            Some(s) if s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit()) => {
                s.parse().ok()
            }
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
    let m = if let Value::Obj(o) = this {
        o.borrow_mut().kind = ObjKind::Map(Vec::new());
        o.clone()
    } else {
        Rc::new(RefCell::new(Obj::new(
            Some(i.p.obj.clone()),
            ObjKind::Map(Vec::new()),
        )))
    };
    match arg(args, 0) {
        Value::Und | Value::Null => {}
        v => {
            for e in i.iter_values(&v)? {
                let k = i.get_prop_str(&e, "0")?;
                let val = i.get_prop_str(&e, "1")?;
                if let ObjKind::Map(items) = &mut m.borrow_mut().kind {
                    if let Some(slot) = items.iter_mut().find(|(k2, _)| same_value_zero(k2, &k)) {
                        slot.1 = val;
                    } else {
                        items.push((k, val));
                    }
                }
            }
        }
    }
    Ok(Value::Obj(m))
}

fn map_get(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let _ = i;
    if let Value::Obj(o) = this {
        if let ObjKind::Map(items) = &o.borrow().kind {
            let k = arg(args, 0);
            for (k2, v) in items {
                if same_value_zero(k2, &k) {
                    return Ok(v.clone());
                }
            }
        }
    }
    Ok(Value::Und)
}

fn map_set(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let _ = i;
    let k = arg(args, 0);
    let v = arg(args, 1);
    if let Value::Obj(o) = this {
        if let ObjKind::Map(items) = &mut o.borrow_mut().kind {
            if let Some(slot) = items.iter_mut().find(|(k2, _)| same_value_zero(k2, &k)) {
                slot.1 = v;
            } else {
                items.push((k, v));
            }
        }
    }
    Ok(this.clone())
}

fn map_has(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let _ = i;
    if let Value::Obj(o) = this {
        if let ObjKind::Map(items) = &o.borrow().kind {
            let k = arg(args, 0);
            return Ok(Value::Bool(
                items.iter().any(|(k2, _)| same_value_zero(k2, &k)),
            ));
        }
    }
    Ok(Value::Bool(false))
}

fn map_delete(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let _ = i;
    if let Value::Obj(o) = this {
        if let ObjKind::Map(items) = &mut o.borrow_mut().kind {
            let k = arg(args, 0);
            let before = items.len();
            items.retain(|(k2, _)| !same_value_zero(k2, &k));
            return Ok(Value::Bool(items.len() != before));
        }
    }
    Ok(Value::Bool(false))
}

fn map_clear(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let _ = i;
    if let Value::Obj(o) = this {
        if let ObjKind::Map(items) = &mut o.borrow_mut().kind {
            items.clear();
        }
    }
    Ok(Value::Und)
}

fn map_for_each(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    if let Value::Obj(o) = this {
        let items = match &o.borrow().kind {
            ObjKind::Map(items) => items.clone(),
            _ => Vec::new(),
        };
        for (k, v) in items {
            i.call(&cb, this_arg.clone(), vec![v, k, this.clone()])?;
        }
    }
    Ok(Value::Und)
}

fn map_keys(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    if let Value::Obj(o) = this {
        if let ObjKind::Map(items) = &o.borrow().kind {
            let ks: Vec<Value> = items.iter().map(|(k, _)| k.clone()).collect();
            return Ok(make_iter_in(ks, i.p.map_iter.clone()));
        }
    }
    Ok(make_iter_in(vec![], i.p.map_iter.clone()))
}

fn map_values(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    if let Value::Obj(o) = this {
        if matches!(&o.borrow().kind, ObjKind::Map(_)) {
            return Ok(make_live_iter(o.clone(), i.p.map_iter.clone(), 4));
        }
    }
    Ok(make_iter_in(vec![], i.p.map_iter.clone()))
}

fn map_entries(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    if let Value::Obj(o) = this {
        if matches!(&o.borrow().kind, ObjKind::Map(_)) {
            return Ok(make_live_iter(o.clone(), i.p.map_iter.clone(), 3));
        }
    }
    Ok(make_iter_in(vec![], i.p.map_iter.clone()))
}

fn map_size(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let _ = i;
    if let Value::Obj(o) = this {
        if let ObjKind::Map(items) = &o.borrow().kind {
            return Ok(Value::Num(items.len() as f64));
        }
    }
    Ok(Value::Num(0.0))
}

fn set_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = if let Value::Obj(o) = this {
        o.borrow_mut().kind = ObjKind::Set(Vec::new());
        o.clone()
    } else {
        Rc::new(RefCell::new(Obj::new(
            Some(i.p.obj.clone()),
            ObjKind::Set(Vec::new()),
        )))
    };
    match arg(args, 0) {
        Value::Und | Value::Null => {}
        v => {
            for it in i.iter_values(&v)? {
                if let ObjKind::Set(items) = &mut s.borrow_mut().kind {
                    if !items.iter().any(|x| same_value_zero(x, &it)) {
                        items.push(it);
                    }
                }
            }
        }
    }
    Ok(Value::Obj(s))
}

fn set_add(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let _ = i;
    let v = arg(args, 0);
    if let Value::Obj(o) = this {
        if let ObjKind::Set(items) = &mut o.borrow_mut().kind {
            if !items.iter().any(|x| same_value_zero(x, &v)) {
                items.push(v);
            }
        }
    }
    Ok(this.clone())
}

fn set_has(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let _ = i;
    if let Value::Obj(o) = this {
        if let ObjKind::Set(items) = &o.borrow().kind {
            let v = arg(args, 0);
            return Ok(Value::Bool(items.iter().any(|x| same_value_zero(x, &v))));
        }
    }
    Ok(Value::Bool(false))
}

fn set_delete(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let _ = i;
    if let Value::Obj(o) = this {
        if let ObjKind::Set(items) = &mut o.borrow_mut().kind {
            let v = arg(args, 0);
            let before = items.len();
            items.retain(|x| !same_value_zero(x, &v));
            return Ok(Value::Bool(items.len() != before));
        }
    }
    Ok(Value::Bool(false))
}

fn set_clear(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let _ = i;
    if let Value::Obj(o) = this {
        if let ObjKind::Set(items) = &mut o.borrow_mut().kind {
            items.clear();
        }
    }
    Ok(Value::Und)
}

fn set_for_each(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    if let Value::Obj(o) = this {
        if let ObjKind::Set(items) = &o.borrow().kind {
            let items = items.clone();
            for v in items {
                i.call(&cb, this_arg.clone(), vec![v.clone(), v, this.clone()])?;
            }
        }
    }
    Ok(Value::Und)
}

fn set_values(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    if let Value::Obj(o) = this {
        if matches!(&o.borrow().kind, ObjKind::Set(_)) {
            return Ok(make_live_iter(o.clone(), i.p.set_iter.clone(), 5));
        }
    }
    Ok(make_iter_in(vec![], i.p.set_iter.clone()))
}

fn set_entries(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    if let Value::Obj(o) = this {
        if matches!(&o.borrow().kind, ObjKind::Set(_)) {
            return Ok(make_live_iter(o.clone(), i.p.set_iter.clone(), 6));
        }
    }
    Ok(make_iter_in(vec![], i.p.set_iter.clone()))
}

fn set_size(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let _ = i;
    if let Value::Obj(o) = this {
        if let ObjKind::Set(items) = &o.borrow().kind {
            return Ok(Value::Num(items.len() as f64));
        }
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
    let mut radix = i.to_num(&arg(args, 1)).unwrap_or(0.0) as u32;
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
                return Ok(Value::Num(f64::INFINITY));
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

fn g_encode_uri_component(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let s = i.to_str(&arg(args, 0))?;
    let mut out = String::new();
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    Ok(Value::Str(Rc::from(out.as_str())))
}

fn g_decode_uri_component(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let s = i.to_str(&arg(args, 0))?;
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut k = 0;
    while k < b.len() {
        if b[k] == b'%' && k + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[k + 1..k + 3], 16) {
                out.push(v);
                k += 3;
                continue;
            }
        }
        out.push(b[k]);
        k += 1;
    }
    Ok(Value::Str(Rc::from(
        String::from_utf8_lossy(&out).to_string().as_str(),
    )))
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
    let f = arg(args, 0);
    if i.is_callable(&f) {
        i.call(&f, Value::Und, vec![])?;
    }
    Ok(Value::Und)
}

fn g_dyn_import(i: &mut Interp, _t: &Value, _a: &[Value]) -> NResult {
    // dynamic import() — unresolved modules yield an empty namespace object
    let ns = Value::Obj(i.new_obj());
    promise_resolve_static(i, &Value::Und, &[ns])
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
    if let ObjKind::Proxy { target: tg, handler } = o.borrow().kind.clone() {
        return match i.proxy_trap(&handler, "get")? {
            Some(trap) => {
                let kv = i.key_to_value(&k);
                i.call(&trap, handler, vec![tg, kv, receiver])
            }
            None => match tg {
                Value::Obj(to) => i.obj_get(&to, &k, &receiver),
                _ => Ok(Value::Und),
            },
        };
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
    if let ObjKind::Proxy { target: tg, handler } = o.borrow().kind.clone() {
        let (tg, h) = (tg, handler);
        return match i.proxy_trap(&h, "defineProperty")? {
            Some(trap) => {
                let r = i.call(&trap, h, vec![tg, i.key_to_value(&key), desc])?;
                Ok(Value::Bool(i.to_bool(&r)))
            }
            None => match &tg {
                Value::Obj(to) => {
                    Ok(Value::Bool(define_prop_from_desc(i, to, key, &desc)?))
                }
                _ => Err(i.type_err("proxy target is not an object")),
            },
        };
    }
    Ok(Value::Bool(define_prop_from_desc(i, &o, key, &desc)?))
}

fn reflect_set_prototype_of(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let target = arg(args, 0);
    let Value::Obj(o) = &target else {
        return Err(i.type_err("Reflect.setPrototypeOf called on non-object"));
    };
    let v = arg(args, 1);
    if !matches!(v, Value::Obj(_) | Value::Null) {
        return Err(i.type_err(
            "Reflect.setPrototypeOf called with non-object proto",
        ));
    }
    Ok(Value::Bool(ord_set_proto(i, o, &v)?))
}

// Reflect mirrors of the Object versions — same operation but the
// target must already be an object (no ToObject coercion)
fn reflect_get_own_prop_desc(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    if !matches!(arg(args, 0), Value::Obj(_)) {
        return Err(i.type_err(
            "Reflect.getOwnPropertyDescriptor called on non-object",
        ));
    }
    obj_get_own_prop_desc(i, _t, args)
}

fn reflect_get_prototype_of(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    if !matches!(arg(args, 0), Value::Obj(_)) {
        return Err(i.type_err(
            "Reflect.getPrototypeOf called on non-object",
        ));
    }
    obj_get_prototype_of(i, _t, args)
}

fn reflect_is_extensible(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let target = arg(args, 0);
    let Value::Obj(o) = &target else {
        return Err(i.type_err(
            "Reflect.isExtensible called on non-object",
        ));
    };
    Ok(Value::Bool(ord_is_extensible(i, o)?))
}

fn reflect_prevent_extensions(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let target = arg(args, 0);
    let Value::Obj(o) = &target else {
        return Err(i.type_err(
            "Reflect.preventExtensions called on non-object",
        ));
    };
    Ok(Value::Bool(ord_prevent_ext(i, o)?))
}

fn reflect_apply(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let f = arg(args, 0);
    if !i.is_callable(&f) {
        return Err(i.type_err("Reflect.apply called on non-function"));
    }
    let this_arg = arg(args, 1);
    let list = to_elems(i, &arg(args, 2))?;
    i.call(&f, this_arg, list)
}

fn reflect_construct(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let f = arg(args, 0);
    if !i.is_callable(&f) {
        return Err(i.type_err("Reflect.construct called on non-function"));
    }
    let list = to_elems(i, &arg(args, 1))?;
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
fn get_method(
    i: &mut Interp,
    v: &Value,
    key: &PropKey,
) -> Result<Option<Value>, Value> {
    match i.get_prop(v, key)? {
        Value::Und | Value::Null => Ok(None),
        m if i.is_callable(&m) => Ok(Some(m)),
        _ => Err(i.type_err("property is not callable")),
    }
}

/// spec IsRegExp: a non-undefined @@match result decides, else the
/// [[RegExpMatcher]] slot.
fn is_regexp(i: &mut Interp, v: &Value) -> Result<bool, Value> {
    if !matches!(v, Value::Obj(_)) {
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
fn species_ctor(i: &mut Interp, rx: &Value) -> Result<Value, Value> {
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
            let reject =
                if p.is_accessor() { p.set.is_none() } else { !p.writable };
            if reject {
                return Err(i.type_err(format!(
                    "Cannot assign to read only property '{key}'"
                )));
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
                Some((a, b)) => Value::Obj(
                    i.new_arr(vec![Value::Num(a as f64), Value::Num(b as f64)]),
                ),
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
            "input", "lastMatch", "lastParen", "leftContext", "rightContext",
            "p1", "p2", "p3", "p4", "p5", "p6", "p7", "p8", "p9",
        ] {
            state_set(&st, k, Value::Und);
        }
        return;
    }
    state_set(&st, "input", Value::Str(Rc::from(chars_string(chars).as_str())));
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
            return Err(
                i.type_err("RegExp exec method must return an object or null")
            );
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
        let f = if matches!(flags, Value::Und) {
            i.get_prop_str(&pattern, "flags")?
        } else {
            flags
        };
        (i.get_prop_str(&pattern, "source")?, f)
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
        return Err(
            i.type_err("RegExp.prototype.exec called on incompatible receiver")
        );
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
    let s = format!(
        "/{}/{}",
        re_coerce_str(i, &src)?,
        re_coerce_str(i, &fl)?
    );
    Ok(Value::Str(Rc::from(s.as_str())))
}

/// `re.compile(pat, flags)` — Annex B: reinitialize in place, return `this`.
/// The receiver must have the [[RegExpMatcher]] slot (B.2.4.1 step 2); a
/// subclass instance compiles fine, plain objects throw.
fn re_compile(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    if re_of(this).is_none() {
        return Err(
            i.type_err("RegExp.prototype.compile called on incompatible receiver")
        );
    }
    let pattern = arg(args, 0);
    let flags = arg(args, 1);
    let (p, f) = if let Some(re) = re_of(&pattern) {
        if !matches!(flags, Value::Und) {
            return Err(i.type_err(
                "Cannot supply flags when compiling a RegExp from a RegExp",
            ));
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
        Some(re) => Ok(Value::Str(Rc::from(
            if re.source.is_empty() {
                "(?:)".to_string()
            } else {
                re.source.clone()
            }
            .as_str(),
        ))),
        None => Err(i
            .type_err("RegExp.prototype.source getter called on incompatible receiver")),
    }
}

/// spec `get flags` — generic: reads `hasIndices`/`global`/`ignoreCase`/
/// `multiline`/`dotAll`/`unicode`/`unicodeSets`/`sticky` in order and
/// appends the flag char for each truthy result.
fn re_get_flags(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    if !matches!(this, Value::Obj(_)) {
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
            Err(i.type_err(
                "RegExp.prototype.unicodeSets getter called on incompatible receiver",
            ))
        }
    }
}

// -- Annex-B legacy statics on %RegExp% -----------------------------------------

/// `this` must be SameValue with this realm's %RegExp% — the legacy
/// getters/setters throw TypeError otherwise (GetLegacyRegExpStaticProperty).
/// On success returns the realm state object.
fn re_static_receiver(
    i: &mut Interp,
    this: &Value,
) -> Result<ObjRef, Value> {
    if matches!(this, Value::Obj(o) if Rc::ptr_eq(o, &re_ctor(i))) {
        Ok(re_state(i))
    } else {
        Err(i.type_err(
            "RegExp static property accessor called on incompatible receiver",
        ))
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
    let c = species_ctor(i, rx)?;
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
        return Err(
            i.type_err("RegExp String Iterator next called on non-object")
        );
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
        return Err(i.type_err(
            "RegExp String Iterator next called on incompatible receiver",
        ));
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
        let named = i.get_prop_str(result, "groups")?;
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
    let c = species_ctor(i, rx)?;
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
