//! Binary views (ArrayBuffer / TypedArray / DataView), text codecs
//! (TextEncoder/TextDecoder), URL + URLSearchParams, and assorted
//! web-platform stubs (Worker, Image, Blob, FormData, XMLHttpRequest,
//! crypto) — all std-only, installed by `builtins::install`.

use crate::builtins::{
    clamp_index, make_iter, merge_sort, promise_resolve_static, to_elems,
};
use crate::interp::*;
use std::cell::RefCell;
use std::rc::Rc;

type NR = Result<Value, Value>;

fn arg(args: &[Value], n: usize) -> Value {
    args.get(n).cloned().unwrap_or(Value::Und)
}

fn met(i: &Interp, o: &ObjRef, name: &str, f: NativeFn) {
    met_a(i, o, name, f, 0.0);
}

/// `met` with an explicit function `.length` (spec method arities).
fn met_a(i: &Interp, o: &ObjRef, name: &str, f: NativeFn, len: f64) {
    let v = i.native(name, f);
    if let Value::Obj(fo) = &v {
        i.set_attr(fo, "length", Value::Num(len), false, true);
    }
    i.set_builtin(o, name, v);
}

fn acc(i: &Interp, o: &ObjRef, name: &str, get: Option<NativeFn>, set: Option<NativeFn>) {
    let g = get.map(|f| i.native(&format!("get {name}"), f));
    let s = set.map(|f| i.native(&format!("set {name}"), f));
    o.borrow_mut()
        .props
        .insert(PropKey::from_str(name), Prop::accessor(g, s));
}

fn plain(proto: Option<ObjRef>) -> ObjRef {
    Rc::new(RefCell::new(Obj::new(proto, ObjKind::Ord)))
}

fn mk_ctor(i: &mut Interp, name: &str, f: NativeFn, proto: &ObjRef) -> Value {
    mk_ctor_len(i, name, f, proto, 0.0)
}

/// built-in constructor function object — spec property attributes:
/// `name`/`length` {writable:false, enumerable:false, configurable:true},
/// `prototype` {writable:false, enumerable:false, configurable:false}.
fn mk_ctor_len(i: &mut Interp, name: &str, f: NativeFn, proto: &ObjRef, len: f64) -> Value {
    let o = Rc::new(RefCell::new(Obj::new(
        Some(i.p.func.clone()),
        ObjKind::Func(Func::Native {
            name: Rc::from(name),
            f,
        }),
    )));
    i.set_attr(&o, "name", Value::Str(Rc::from(name)), false, true);
    i.set_attr(&o, "length", Value::Num(len), false, true);
    o.borrow_mut().props.insert(
        PropKey::from_str("prototype"),
        Prop {
            val: Value::Obj(proto.clone()),
            writable: false,
            enumerable: false,
            configurable: false,
            get: None,
            set: None,
        },
    );
    i.set_builtin(proto, "constructor", Value::Obj(o.clone()));
    Value::Obj(o)
}

fn gdef(i: &mut Interp, name: &str, v: Value) {
    let g = i.global.clone();
    i.declare(&g, name, v.clone(), true);
    // also an own property on the global object — builtin globals are
    // {W:true, E:false, C:true} per spec 18.3
    i.p.global.borrow_mut().props.insert(
        PropKey::from_str(name),
        Prop {
            val: v,
            writable: true,
            enumerable: false,
            configurable: true,
            get: None,
            set: None,
        },
    );
}

/// `Ctor.prototype` looked up from the global scope (for objects created
/// by internals rather than `new`).
fn global_ctor_proto(i: &mut Interp, name: &str) -> Option<ObjRef> {
    let g = i.global.clone();
    let v = i.env_lookup(&g, name).ok()??;
    if let Value::Obj(o) = &v {
        if let Some(Value::Obj(p)) = i.obj_get_raw(o, &PropKey::from_str("prototype")) {
            return Some(p.clone());
        }
    }
    None
}

fn hid(i: &Interp, o: &ObjRef, key: &str, v: Value) {
    i.set_builtin(o, key, v);
}

// ===========================================================================
// install
// ===========================================================================

pub fn install(i: &mut Interp) {
    install_typed_arrays(i);
    install_array_buffer(i);
    install_data_view(i);
    install_text_codecs(i);
    install_url(i);
    install_stubs(i);
    install_host_hooks(i);
}

// ===========================================================================
// host hooks ($262 — test262-style harness API)
// ===========================================================================

fn install_host_hooks(i: &mut Interp) {
    let o = plain(Some(i.p.obj.clone()));
    met_a(i, &o, "detachArrayBuffer", h_detach_buffer, 1.0);
    gdef(i, "$262", Value::Obj(o));
}

/// `$262.detachArrayBuffer(buf)` — DetachArrayBuffer: byte length → 0,
/// all views become detached / out-of-bounds.
fn h_detach_buffer(i: &mut Interp, _t: &Value, args: &[Value]) -> NR {
    let Value::Obj(o) = arg(args, 0) else {
        return Err(i.type_err("detachArrayBuffer expects an ArrayBuffer"));
    };
    let Some(b) = buffer_bytes(&o) else {
        return Err(i.type_err("detachArrayBuffer expects an ArrayBuffer"));
    };
    if is_shared(&o) {
        return Err(i.type_err("SharedArrayBuffer cannot be detached"));
    }
    b.borrow_mut().clear();
    o.borrow_mut().detached = true;
    Ok(Value::Und)
}

// ===========================================================================
// TypedArray
// ===========================================================================

const TA_CLASSES: [(&str, TAK); 9] = [
    ("Uint8Array", TAK::U8),
    ("Uint8ClampedArray", TAK::U8C),
    ("Int8Array", TAK::I8),
    ("Uint16Array", TAK::U16),
    ("Int16Array", TAK::I16),
    ("Uint32Array", TAK::U32),
    ("Int32Array", TAK::I32),
    ("Float32Array", TAK::F32),
    ("Float64Array", TAK::F64),
];

fn u8_ctor(i: &mut Interp, t: &Value, a: &[Value]) -> NR {
    ta_ctor(i, t, a, TAK::U8, false)
}
fn u8c_ctor(i: &mut Interp, t: &Value, a: &[Value]) -> NR {
    ta_ctor(i, t, a, TAK::U8C, false)
}
fn i8_ctor(i: &mut Interp, t: &Value, a: &[Value]) -> NR {
    ta_ctor(i, t, a, TAK::I8, false)
}
fn u16_ctor(i: &mut Interp, t: &Value, a: &[Value]) -> NR {
    ta_ctor(i, t, a, TAK::U16, false)
}
fn i16_ctor(i: &mut Interp, t: &Value, a: &[Value]) -> NR {
    ta_ctor(i, t, a, TAK::I16, false)
}
fn u32_ctor(i: &mut Interp, t: &Value, a: &[Value]) -> NR {
    ta_ctor(i, t, a, TAK::U32, false)
}
fn i32_ctor(i: &mut Interp, t: &Value, a: &[Value]) -> NR {
    ta_ctor(i, t, a, TAK::I32, false)
}
fn f32_ctor(i: &mut Interp, t: &Value, a: &[Value]) -> NR {
    ta_ctor(i, t, a, TAK::F32, false)
}
fn f64_ctor(i: &mut Interp, t: &Value, a: &[Value]) -> NR {
    ta_ctor(i, t, a, TAK::F64, false)
}
/// BigInt64Array / BigUint64Array — the engine has no real bigint
/// representation, so these behave as 8-byte float arrays (bigint
/// stand-in objects still coerce via their `__bigint` value). A
/// hidden `__name` slot records the real TypedArrayName since both
/// share TAK::F64 — `ta_intrinsic_name` resolves it.
fn big_ta_ctor(i: &mut Interp, t: &Value, a: &[Value], name: &'static str) -> NR {
    // `ctor_big` = true — spec [[ContentType]] comes from the invoked
    // constructor, not the receiver's prototype chain (a custom
    // newTarget.prototype can replace that chain entirely).
    let r = ta_ctor(i, t, a, TAK::F64, true)?;
    if let Value::Obj(o) = &r {
        hid(i, o, "__name", Value::Str(Rc::from(name)));
    }
    Ok(r)
}
fn b64_ctor(i: &mut Interp, t: &Value, a: &[Value]) -> NR {
    big_ta_ctor(i, t, a, "BigInt64Array")
}
fn bu64_ctor(i: &mut Interp, t: &Value, a: &[Value]) -> NR {
    big_ta_ctor(i, t, a, "BigUint64Array")
}

/// `[[TypedArrayName]]` — BigInt stand-ins share TAK::F64 and carry
/// their real name in the hidden `__name` slot instead.
fn ta_intrinsic_name(v: &Value, kind: TAK) -> String {
    if let Value::Obj(o) = v {
        if let Some(p) = o.borrow().props.get_str("__name") {
            if let Value::Str(s) = &p.val {
                return s.to_string();
            }
        }
    }
    kind.type_name().to_string()
}

/// `%BigInt.prototype%.valueOf` — stand-ins carry their magnitude in a
/// non-enumerable `__bigint` slot; returning it lets Number() /
/// ToNumber treat them like real bigints.
fn bigint_value_of(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    if let Value::Obj(o) = this {
        if let Some(v) = o.borrow().props.get_str("__bigint").map(|p| p.val.clone()) {
            return Ok(v);
        }
    }
    Err(i.type_err("not a BigInt"))
}

fn bigint_to_string(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    if let Value::Obj(o) = this {
        if let Some(v) = o.borrow().props.get_str("__bigint").map(|p| p.val.clone()) {
            return i.to_str(&v).map(|s| Value::Str(Rc::from(s.as_str())));
        }
    }
    Err(i.type_err("not a BigInt"))
}

const TA_CTORS: [NativeFn; 9] = [
    u8_ctor, u8c_ctor, i8_ctor, u16_ctor, i16_ctor, u32_ctor, i32_ctor, f32_ctor, f64_ctor,
];

fn install_typed_arrays(i: &mut Interp) {
    // %TypedArray%.prototype — all methods live here; per-class protos
    // chain to it.
    let shared = plain(Some(i.p.obj.clone()));
    for (n, f, arity) in [
        ("set", m_ta_set as NativeFn, 1.0),
        ("subarray", m_ta_subarray, 2.0),
        ("slice", m_ta_slice, 2.0),
        ("fill", m_ta_fill, 1.0),
        ("copyWithin", m_ta_copy_within, 2.0),
        ("reverse", m_ta_reverse, 0.0),
        ("sort", m_ta_sort, 1.0),
        ("indexOf", m_ta_index_of, 1.0),
        ("lastIndexOf", m_ta_last_index_of, 1.0),
        ("includes", m_ta_includes, 1.0),
        ("join", m_ta_join, 1.0),
        ("toLocaleString", m_ta_to_locale_string, 0.0),
        ("at", m_ta_at, 1.0),
        ("map", m_ta_map, 1.0),
        ("filter", m_ta_filter, 1.0),
        ("forEach", m_ta_for_each, 1.0),
        ("reduce", m_ta_reduce, 1.0),
        ("reduceRight", m_ta_reduce_right, 1.0),
        ("find", m_ta_find, 1.0),
        ("findIndex", m_ta_find_index, 1.0),
        ("findLast", m_ta_find_last, 1.0),
        ("findLastIndex", m_ta_find_last_index, 1.0),
        ("some", m_ta_some, 1.0),
        ("every", m_ta_every, 1.0),
        ("keys", m_ta_keys, 0.0),
        ("values", m_ta_values, 0.0),
        ("entries", m_ta_entries, 0.0),
        ("toReversed", m_ta_to_reversed, 0.0),
        ("toSorted", m_ta_to_sorted, 1.0),
        ("with", m_ta_with, 2.0),
    ] {
        let v = i.native(n, f);
        if let Value::Obj(fo) = &v {
            i.set_attr(fo, "length", Value::Num(arity), false, true);
        }
        i.set_builtin(&shared, n, v);
    }
    // %TypedArray%.prototype.toString === Array.prototype.toString
    if let Some(ts) = i.obj_get_raw(&i.p.array.clone(), &PropKey::from_str("toString")) {
        i.set_builtin(&shared, "toString", ts);
    }
    acc(i, &shared, "length", Some(ta_get_len), None);
    acc(i, &shared, "byteLength", Some(ta_get_byte_len), None);
    acc(i, &shared, "byteOffset", Some(ta_get_byte_off), None);
    acc(i, &shared, "buffer", Some(ta_get_buffer), None);
    // @@toStringTag — accessor producing the receiver's TypedArrayName
    // (spec: undefined for non-TA receivers).
    let tag_get = i.native("get [Symbol.toStringTag]", ta_get_tag);
    shared.borrow_mut().props.insert(
        PropKey::Sym(SYM_TOSTRINGTAG),
        Prop::accessor(Some(tag_get), None),
    );
    // Symbol.iterator === values
    let values_fn = i
        .obj_get_raw(&shared, &PropKey::from_str("values"))
        .unwrap_or(Value::Und);
    shared
        .borrow_mut()
        .props
        .insert(PropKey::Sym(SYM_ITERATOR), Prop::builtin(values_fn));

    // %TypedArray% — the abstract intrinsic constructor. Concrete
    // ctors chain to it (`Object.getPrototypeOf(Int8Array)`), so the
    // shared @@species getter and statics resolve by inheritance.
    let ta_intr = Rc::new(RefCell::new(Obj::new(
        Some(i.p.func.clone()),
        ObjKind::Func(Func::Native {
            name: Rc::from("TypedArray"),
            f: ta_intr_ctor,
        }),
    )));
    i.set_attr(&ta_intr, "length", Value::Num(0.0), false, true);
    i.set_attr(
        &ta_intr,
        "name",
        Value::Str(Rc::from("TypedArray")),
        false,
        true,
    );
    // .prototype — {writable:false, enumerable:false, configurable:false}
    ta_intr.borrow_mut().props.insert(
        PropKey::from_str("prototype"),
        Prop {
            val: Value::Obj(shared.clone()),
            writable: false,
            enumerable: false,
            configurable: false,
            get: None,
            set: None,
        },
    );
    i.set_builtin(&shared, "constructor", Value::Obj(ta_intr.clone()));
    {
        let of = i.native("of", ta_static_of);
        i.set_builtin(&ta_intr, "of", of);
        let from = i.native("from", ta_static_from);
        if let Value::Obj(fo) = &from {
            i.set_attr(fo, "length", Value::Num(1.0), false, true);
        }
        i.set_builtin(&ta_intr, "from", from);
        let sp = i.native("get [Symbol.species]", ta_get_species);
        ta_intr.borrow_mut().props.insert(
            PropKey::Sym(SYM_SPECIES),
            Prop::accessor(Some(sp), None),
        );
    }

    for ((name, kind), cf) in TA_CLASSES.iter().zip(TA_CTORS.iter()) {
        let proto = plain(Some(shared.clone()));
        let ctor = i.native(name, *cf);
        if let Value::Obj(co) = &ctor {
            co.borrow_mut().proto = Some(ta_intr.clone());
            co.borrow_mut().props.insert(
                PropKey::from_str("prototype"),
                Prop {
                    val: Value::Obj(proto.clone()),
                    writable: false,
                    enumerable: false,
                    configurable: false,
                    get: None,
                    set: None,
                },
            );
            i.set_attr(
                co,
                "BYTES_PER_ELEMENT",
                Value::Num(kind.size() as f64),
                false,
                false,
            );
            // every concrete TypedArray ctor declares (buffer,
            // byteOffset, length) → .length = 3
            i.set_attr(co, "length", Value::Num(3.0), false, true);
        }
        i.set_attr(
            &proto,
            "BYTES_PER_ELEMENT",
            Value::Num(kind.size() as f64),
            false,
            false,
        );
        i.set_builtin(&proto, "constructor", ctor.clone());
        gdef(i, name, ctor);
    }
    // bigint stand-in objects get coercible so they behave like real
    // bigints through ToPrimitive → valueOf.
    {
        let bp = i.p.bigint.clone();
        i.set_builtin(&bp, "valueOf", i.native("valueOf", bigint_value_of));
        i.set_builtin(&bp, "toString", i.native("toString", bigint_to_string));
        bp.borrow_mut().props.insert(
            PropKey::Sym(SYM_TOSTRINGTAG),
            Prop::builtin(Value::Str(Rc::from("BigInt"))),
        );
    }
    // tolerant stubs — exist so `typeof BigInt64Array === "function"`;
    // they behave as 8-byte float arrays (see big_ta_ctor).
    for (name, cf) in [
        ("BigInt64Array", b64_ctor as NativeFn),
        ("BigUint64Array", bu64_ctor),
    ] {
        let proto = plain(Some(shared.clone()));
        // spec [[TypedArrayName]] on the proto too — `ta_is_big` uses
        // it to detect bigint element kind before the instance slot is
        // installed (inside the constructor body) and for subclasses.
        hid(i, &proto, "__name", Value::Str(Rc::from(name)));
        let ctor = i.native(name, cf);
        if let Value::Obj(co) = &ctor {
            co.borrow_mut().proto = Some(ta_intr.clone());
            co.borrow_mut().props.insert(
                PropKey::from_str("prototype"),
                Prop {
                    val: Value::Obj(proto.clone()),
                    writable: false,
                    enumerable: false,
                    configurable: false,
                    get: None,
                    set: None,
                },
            );
            i.set_attr(co, "BYTES_PER_ELEMENT", Value::Num(8.0), false, false);
            i.set_attr(co, "length", Value::Num(3.0), false, true);
            i.set_builtin(&proto, "constructor", ctor.clone());
        }
        gdef(i, name, ctor);
    }
}

/// %TypedArray% itself is an abstract constructor — always throws.
fn ta_intr_ctor(i: &mut Interp, _t: &Value, _a: &[Value]) -> NR {
    Err(i.type_err("The TypedArray constructor is abstract"))
}

/// `get %TypedArray%[Symbol.species]` — returns `this`.
fn ta_get_species(_i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    Ok(this.clone())
}

/// `get %TypedArray%.prototype[Symbol.toStringTag]` — receiver's
/// TypedArrayName, or undefined for non-TypedArray receivers.
fn ta_get_tag(_i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    Ok(match ta_parts(this) {
        Some(t) => Value::Str(Rc::from(ta_intrinsic_name(this, t.kind))),
        None => Value::Und,
    })
}

fn ta_get_len(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let t = ta_parts(this).ok_or_else(|| {
        i.type_err("Method called on incompatible receiver (expected TypedArray)")
    })?;
    // detached / out-of-bounds views report length 0
    Ok(Value::Num(t.eff().unwrap_or(0) as f64))
}

fn ta_get_byte_len(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let t = ta_parts(this).ok_or_else(|| {
        i.type_err("Method called on incompatible receiver (expected TypedArray)")
    })?;
    Ok(Value::Num(
        t.eff().map(|n| n * t.kind.size()).unwrap_or(0) as f64
    ))
}

fn ta_get_byte_off(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let t = ta_parts(this).ok_or_else(|| {
        i.type_err("Method called on incompatible receiver (expected TypedArray)")
    })?;
    Ok(Value::Num(if t.eff().is_some() {
        t.off as f64
    } else {
        0.0
    }))
}

fn ta_get_buffer(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let t = ta_parts(this).ok_or_else(|| {
        i.type_err("Method called on incompatible receiver (expected TypedArray)")
    })?;
    Ok(Value::Obj(t.buf))
}

/// `ObjKind::Typed` internals pulled into a single value.
#[derive(Clone)]
struct Tap {
    buf: ObjRef,
    off: usize,
    len: usize,
    kind: TAK,
    track: bool,
}

impl Tap {
    /// Live element capacity — `None` when the view is detached or
    /// out-of-bounds (resizable buffer shrunk past the view's range).
    fn eff(&self) -> Option<usize> {
        typed_state(&self.buf, self.off, self.len, self.kind, self.track)
    }
    /// spec IntegerIndexedElementGet — `None` reads as `undefined`.
    fn get(&self, k: usize) -> Option<f64> {
        if !self.eff().is_some_and(|n| k < n) {
            return None;
        }
        let b = buffer_bytes(&self.buf)?;
        let bb = b.borrow();
        Some(self.kind.read(&bb, self.off + k * self.kind.size()))
    }
    /// spec IntegerIndexedElementSet's tail — in-bounds writes hit the
    /// immutable check (TypeError); detached / out-of-bounds writes are
    /// silently dropped.
    fn set_num(&self, i: &mut Interp, k: usize, v: f64) -> Result<(), Value> {
        if self.eff().is_some_and(|n| k < n) {
            if self.buf.borrow().immutable {
                return Err(i.type_err("Cannot write to an immutable ArrayBuffer"));
            }
            if let Some(b) = buffer_bytes(&self.buf) {
                self.kind
                    .write(&mut b.borrow_mut(), self.off + k * self.kind.size(), v);
            }
        }
        Ok(())
    }
}

/// `ObjKind::Typed` internals for a TypedArray value.
fn ta_parts(v: &Value) -> Option<Tap> {
    if let Value::Obj(o) = v {
        if let ObjKind::Typed {
            buf,
            off,
            len,
            kind,
            track,
        } = &o.borrow().kind
        {
            return Some(Tap {
                buf: buf.clone(),
                off: *off,
                len: *len,
                kind: *kind,
                track: *track,
            });
        }
    }
    None
}

/// All live elements of a TypedArray as f64s (empty when detached/OOB).
fn ta_f64s(v: &Value) -> Vec<f64> {
    let Some(t) = ta_parts(v) else {
        return Vec::new();
    };
    let Some(n) = t.eff() else {
        return Vec::new();
    };
    (0..n).filter_map(|k| t.get(k)).collect()
}

/// integer-index `k` as a PropKey — for Get/Set on TAs and array-likes.
fn ta_elem_key(k: usize) -> PropKey {
    PropKey::from_str(&k.to_string())
}

/// live element read via the integer-indexed exotic [[Get]] path.
fn ta_elem(i: &mut Interp, this: &Value, k: usize) -> Result<Value, Value> {
    i.get_prop(this, &ta_elem_key(k))
}

/// wrap a raw element value for JS visibility — bigint-kind arrays
/// expose bigint stand-in objects; everything else passes through.
pub(crate) fn ta_pubify(i: &mut Interp, this: &Value, v: Value) -> Value {
    if let Value::Num(n) = &v {
        if ta_is_big(this) {
            return i.bigint_obj(*n);
        }
    }
    v
}

/// element value as JS code should observe it — bigint-kind arrays
/// wrap the raw f64 magnitude in the engine's bigint stand-in object
/// (used for `.at`, callback arguments, iterator values, `find`).
fn ta_pub_elem(i: &mut Interp, this: &Value, k: usize) -> Result<Value, Value> {
    let v = ta_elem(i, this, k)?;
    Ok(ta_pubify(i, this, v))
}

/// element-vs-searchElement comparison honoring [[ContentType]] —
/// a bigint-kind element only matches a bigint stand-in argument
/// (magnitude compared), and a number-kind element never matches a
/// stand-in. `svz` picks SameValueZero (includes) vs strict (indexOf).
fn ta_elem_matches(v: &Value, needle: &Value, big: bool, svz: bool) -> bool {
    if big {
        return match bigint_magnitude(needle) {
            Some(n) => matches!(v, Value::Num(e) if *e == n),
            None => false,
        };
    }
    if bigint_magnitude(needle).is_some() {
        return false;
    }
    if svz {
        ta_same_value_zero(v, needle)
    } else {
        ta_strict_eq(v, needle)
    }
}

/// the element conversion performed inside integer-indexed [[Set]]
/// (ToNumber / ToBigInt) — rejects the mismatched primitive before
/// delegating to the ordinary property write used by species-created
/// destinations and `of`/`from`.
#[allow(dead_code)]
fn ta_store_val(
    i: &mut Interp,
    a: &Value,
    k: usize,
    v: &Value,
    big: bool,
) -> Result<(), Value> {
    if !big && bigint_magnitude(v).is_some() {
        return Err(i.type_err("Cannot convert a BigInt value to a number"));
    }
    if big && matches!(v, Value::Num(_)) {
        return Err(i.type_err("Cannot convert a number to a BigInt"));
    }
    i.set_prop(a, ta_elem_key(k), v.clone())
}

/// ? ToIntegerOrInfinity — NaN → 0, fraction truncated, ±∞ preserved.
fn ta_to_int(i: &mut Interp, v: &Value) -> Result<f64, Value> {
    let n = i.to_num(v)?;
    Ok(if n.is_nan() { 0.0 } else { n.trunc() })
}

/// spec ToIndex — ToIntegerOrInfinity then assert 0 ≤ n < 2^53.
/// NaN → +0; negative / ±∞ / ≥ 2^53 → RangeError.
fn to_index(i: &mut Interp, v: &Value) -> Result<usize, Value> {
    let n = i.to_num(v)?.trunc();
    if n.is_nan() {
        return Ok(0);
    }
    if !n.is_finite() || n < 0.0 || n >= 9007199254740992.0 {
        return Err(i.range_err("index out of range"));
    }
    Ok(n as usize)
}

/// magnitude of a bigint stand-in object (the non-enumerable
/// `__bigint` slot installed by `Interp::bigint_obj`).
pub(crate) fn bigint_magnitude(v: &Value) -> Option<f64> {
    if let Value::Obj(o) = v {
        if let Some(Value::Num(n)) =
            o.borrow().props.get_str("__bigint").map(|p| p.val.clone())
        {
            return Some(n);
        }
    }
    None
}

/// spec StringToBigInt — a StringIntegerLiteral (decimal / 0x / 0o /
/// 0b, optional sign for decimal only); empty → 0n; malformed →
/// SyntaxError. Values are approximated as f64.
fn str_to_bigint_num(i: &mut Interp, s: &str) -> Result<f64, Value> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(0.0);
    }
    let (sign, body) = match t.strip_prefix('-') {
        Some(r) => (-1.0, r),
        None => match t.strip_prefix('+') {
            Some(r) => (1.0, r),
            None => (1.0, t),
        },
    };
    let signed = sign < 0.0 || t.starts_with('+');
    let (digits, radix) = if let Some(h) =
        body.strip_prefix("0x").or_else(|| body.strip_prefix("0X"))
    {
        (h, 16)
    } else if let Some(h) =
        body.strip_prefix("0o").or_else(|| body.strip_prefix("0O"))
    {
        (h, 8)
    } else if let Some(h) =
        body.strip_prefix("0b").or_else(|| body.strip_prefix("0B"))
    {
        (h, 2)
    } else {
        (body, 10)
    };
    if digits.is_empty()
        || !digits.chars().all(|c| c.is_digit(radix))
        || (signed && radix != 10)
    {
        return Err(i.syntax_err("Cannot convert string to a BigInt"));
    }
    let n = u128::from_str_radix(digits, radix)
        .map(|v| v as f64)
        .unwrap_or(f64::INFINITY);
    Ok(sign * n)
}

/// spec element conversion — ToNumber for number-kind TypedArrays and
/// a ToBigInt approximation for bigint-kind ones (stand-in objects
/// carry their magnitude in `__bigint`; real engines throw TypeError
/// converting BigInt↔Number, which we emulate).
pub(crate) fn ta_elem_num(i: &mut Interp, v: &Value, big: bool) -> Result<f64, Value> {
    if big {
        return match v {
            Value::Num(_) | Value::Und | Value::Null | Value::Sym(..) => {
                Err(i.type_err("Cannot convert value to a BigInt"))
            }
            Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
            Value::Str(s) => str_to_bigint_num(i, s),
            Value::Obj(o) => match bigint_magnitude(v) {
                Some(n) => Ok(n),
                None => {
                    // ToPrimitive (number hint) then retry the conversion
                    let p = i.to_primitive_hint(v, o.clone(), "number")?;
                    ta_elem_num(i, &p, true)
                }
            },
        };
    }
    if bigint_magnitude(v).is_some() {
        return Err(i.type_err("Cannot convert a BigInt value to a number"));
    }
    i.to_num(v)
}

/// coerce a value for a (possibly bigint-kind) typed-array element
/// write — ToNumber/ToBigInt + the ToBigInt64/ToBigUint64 mod-2^64 wrap.
pub(crate) fn ta_elem_write(
    i: &mut Interp,
    this: &Value,
    v: &Value,
) -> Result<f64, Value> {
    let big = ta_is_big(this);
    let n = ta_elem_num(i, v, big)?;
    Ok(if big {
        // f64 stand-ins approximate the arbitrary-precision spec: the
        // wrap keeps at least the magnitude's sign/parity right.
        const M: f64 = 18446744073709551616.0; // 2^64
        let mut r = n.rem_euclid(M);
        if ta_intrinsic_name(this, TAK::F64) == "BigInt64Array" && r >= M / 2.0 {
            r -= M;
        }
        r
    } else {
        n
    })
}

/// spec [[ContentType]] check — bigint-kind TypedArrays carry the
/// hidden `__name` slot either directly (installed by `big_ta_ctor`)
/// or on their prototype (installed at bootstrap for the intrinsic
/// names); a proto-chain walk covers both plus subclasses.
pub(crate) fn ta_is_big(v: &Value) -> bool {
    let Value::Obj(o) = v else {
        return false;
    };
    let mut cur = Some(o.clone());
    while let Some(c) = cur {
        if c
            .borrow()
            .props
            .get_str("__name")
            .is_some_and(|p| matches!(p.val, Value::Str(_)))
        {
            return true;
        }
        cur = c.borrow().proto.clone();
    }
    false
}

/// SameValueZero for element comparisons (includes).
fn ta_same_value_zero(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Num(x), Value::Num(y)) => x == y || (x.is_nan() && y.is_nan()),
        _ => ta_strict_eq(a, b),
    }
}

/// strict equality for element comparisons (indexOf / lastIndexOf).
fn ta_strict_eq(a: &Value, b: &Value) -> bool {
    use Value::*;
    match (a, b) {
        (Und, Und) | (Null, Null) => true,
        (Bool(x), Bool(y)) => x == y,
        (Num(x), Num(y)) => x == y,
        (Str(x), Str(y)) => x == y,
        (Sym(x, _), Sym(y, _)) => x == y,
        (Obj(x), Obj(y)) => Rc::ptr_eq(x, y),
        _ => false,
    }
}

fn mk_typed(
    proto: Option<ObjRef>,
    buf: ObjRef,
    off: usize,
    len: usize,
    kind: TAK,
    track: bool,
) -> ObjRef {
    Rc::new(RefCell::new(Obj::new(
        proto,
        ObjKind::Typed {
            buf,
            off,
            len,
            kind,
            track,
        },
    )))
}

fn mk_buffer(i: &Interp, data: Vec<u8>) -> ObjRef {
    Rc::new(RefCell::new(Obj::new(
        Some(i.p.arraybuffer.clone()),
        ObjKind::Bytes(Rc::new(RefCell::new(data))),
    )))
}

/// buffer of `kind`-encoded `elems` → (buffer obj, elem count)
fn mk_buf_of(i: &Interp, elems: &[f64], kind: TAK) -> (ObjRef, usize) {
    let sz = kind.size();
    let buf = mk_buffer(i, vec![0u8; elems.len() * sz]);
    if let Some(b) = buffer_bytes(&buf) {
        let mut bb = b.borrow_mut();
        for (k, v) in elems.iter().enumerate() {
            kind.write(&mut bb, k * sz, *v);
        }
    }
    (buf, elems.len())
}

/// `new <matching intrinsic ctor>(len)` — spec TypedArrayCreateSameType
/// (toReversed / toSorted / with) and the default species path.
/// `src` supplies `[[TypedArrayName]]` (BigInt stand-ins differ from
/// their element kind).
fn ta_fresh_typed(i: &mut Interp, src: &Value, kind: TAK, len: usize) -> NR {
    let g = i.global.clone();
    let cv = i
        .env_lookup(&g, &ta_intrinsic_name(src, kind))?
        .unwrap_or(Value::Und);
    i.construct(&cv, vec![Value::Num(len as f64)], None)
}

/// spec SpeciesConstructor — `this.constructor[@@species]`;
/// `Ok(None)` selects the default constructor for the TA's own kind.
fn ta_species_ctor(i: &mut Interp, this: &Value) -> Result<Option<Value>, Value> {
    let c = i.get_prop_str(this, "constructor")?;
    if matches!(c, Value::Und) {
        return Ok(None);
    }
    if !matches!(c, Value::Obj(_)) {
        return Err(i.type_err("constructor is not an object"));
    }
    let s = i.get_prop(&c, &PropKey::Sym(SYM_SPECIES))?;
    match s {
        Value::Und | Value::Null => Ok(None),
        s if i.is_constructor(&s) => Ok(Some(s)),
        _ => Err(i.type_err("constructor[Symbol.species] is not a constructor")),
    }
}

/// spec TypedArraySpeciesCreate + the TypedArrayCreateFromConstructor
/// validation: construct `ctor(args)` (default = intrinsic of `kind`),
/// require a valid TypedArray result. `min_len` enforces the
/// single-number-argument length check used by slice/map/filter.
fn ta_species_result(
    i: &mut Interp,
    this: &Value,
    kind: TAK,
    args: Vec<Value>,
    min_len: Option<usize>,
) -> NR {
    let result = match ta_species_ctor(i, this)? {
        Some(c) => i.construct(&c, args, None)?,
        None => {
            let g = i.global.clone();
            let cv = i
                .env_lookup(&g, &ta_intrinsic_name(this, kind))?
                .unwrap_or(Value::Und);
            i.construct(&cv, args, None)?
        }
    };
    let Some(t) = ta_parts(&result) else {
        return Err(i.type_err("species constructor did not return a TypedArray"));
    };
    let Some(eff) = t.eff() else {
        return Err(i.type_err(
            "species constructor returned a detached or out-of-bounds TypedArray",
        ));
    };
    // spec TypedArrayCreate: a single-Number argument list requires the
    // result's TypedArrayLength — the live computed length for tracking
    // views — to be at least the requested count.
    if let Some(n) = min_len {
        if eff < n {
            return Err(i.type_err(
                "species constructor returned a TypedArray that is too short",
            ));
        }
    }
    // spec TypedArraySpeciesCreate step: the result's [[ContentType]]
    // must match the exemplar's — bigint and number TypedArrays are
    // never interchangeable.
    if ta_is_big(&result) != ta_is_big(this) {
        return Err(i.type_err(
            "species constructor returned a TypedArray with a different content type",
        ));
    }
    Ok(result)
}

/// `new XxxArray(...)` — concrete element-kind constructor.
/// `ctor_big` is spec [[ContentType]]: `true` only for the BigInt
/// TypedArray constructors (it cannot be derived from the receiver's
/// proto — a custom newTarget.prototype can replace that chain).
fn ta_ctor(i: &mut Interp, this: &Value, args: &[Value], kind: TAK, ctor_big: bool) -> NR {
    // `new_target_native` stays set while a native ctor body runs — a
    // bare `Uint8Array(...)` inside a getter/species fn must still
    // throw. Requiring `this` to be the freshly allocated Ordinary
    // object distinguishes a real [[Construct]] from a plain call.
    let fresh_this = matches!(this, Value::Obj(o)
        if matches!(o.borrow().kind, ObjKind::Ord) && o.borrow().props.is_empty());
    if !(i.new_target_native.is_some() && fresh_this) {
        return Err(i.type_err("TypedArray constructor cannot be invoked without 'new'"));
    }
    // spec OrdinaryCreateFromConstructor happens before argument
    // processing — `construct` resolved the proto without invoking
    // getters, so replay the observable Get(newTarget,"prototype")
    // (and adopt an object result): a getter may detach or resize a
    // source buffer.
    apply_new_target_proto(i, this)?;
    let sz = kind.size();
    let src = arg(args, 0);
    // spec [[ContentType]] — from the invoked constructor, not the
    // receiver's proto chain (see `ctor_big`).
    let target_big = ctor_big;
    // (buffer obj, byte offset, elem count, length-tracking)
    let (buf, off, len, track): (ObjRef, usize, usize, bool) = match &src {
        Value::Obj(o) => {
            // a bigint stand-in reaches ToIndex → spec TypeError
            if bigint_magnitude(&src).is_some() {
                return Err(i.type_err("Cannot convert a BigInt value to a number"));
            }
            let kind_of_src = o.borrow().kind.clone();
            match kind_of_src {
                ObjKind::Bytes(b) => {
                    let resizable = o.borrow().rab_max.is_some();
                    // spec InitializeTypedArrayFromArrayBuffer — both
                    // ToIndex coercions run before the detached check.
                    let off = match arg(args, 1) {
                        Value::Und => 0,
                        v => to_index(i, &v)?,
                    };
                    let explen = match arg(args, 2) {
                        Value::Und => None,
                        v => Some(to_index(i, &v)?),
                    };
                    if o.borrow().detached {
                        return Err(i.type_err("Cannot construct a TypedArray on a detached ArrayBuffer"));
                    }
                    let blen = b.borrow().len();
                    if off % sz != 0 || off > blen {
                        return Err(i.range_err("start offset of typed array is out of bounds"));
                    }
                    let (len, track) = match explen {
                        None => {
                            if resizable {
                                // length-tracking view — no alignment
                                // constraint on the initial byte size
                                ((blen - off) / sz, true)
                            } else {
                                if (blen - off) % sz != 0 {
                                    return Err(
                                        i.range_err("byte length of typed array is not aligned")
                                    );
                                }
                                ((blen - off) / sz, false)
                            }
                        }
                        Some(n) => {
                            if n.saturating_mul(sz) > blen - off {
                                return Err(i.range_err("length of typed array out of bounds"));
                            }
                            (n, false)
                        }
                    };
                    (o.clone(), off, len, track)
                }
                ObjKind::Typed { .. } => {
                    // InitializeTypedArrayFromTypedArray — a detached or
                    // out-of-bounds source throws; differing content
                    // types (bigint vs number) also throw.
                    let s = ta_parts(&src).unwrap();
                    if s.eff().is_none() {
                        return Err(i.type_err("source TypedArray is detached or out of bounds"));
                    }
                    if ta_is_big(&src) != target_big {
                        return Err(
                            i.type_err("Cannot mix BigInt and non-BigInt TypedArrays")
                        );
                    }
                    let elems = ta_f64s(&src);
                    let (buf, n) = mk_buf_of(i, &elems, kind);
                    (buf, 0, n, false)
                }
                _ => {
                    // array-like / iterable — spec GetMethod: undefined
                    // or null means "not iterable"; a non-callable
                    // method is a TypeError.
                    let iterm = i.get_prop(&src, &PropKey::Sym(SYM_ITERATOR))?;
                    // spec IterableToList + InitializeTypedArrayFromList
                    // collect every value before the per-element number
                    // conversions; converting inline is equivalent.
                    let convert = |items: Vec<Value>,
                                   i: &mut Interp|
                     -> Result<Vec<f64>, Value> {
                        let mut elems = Vec::with_capacity(items.len());
                        for it in items {
                            elems.push(ta_elem_num(i, &it, target_big)?);
                        }
                        Ok(elems)
                    };
                    let elems: Vec<f64> =
                        if matches!(iterm, Value::Und | Value::Null) {
                            // engine-internal array/iterator kinds may carry
                            // elements without a `length` prop — reuse the
                            // shared drain for those.
                            let internal =
                                matches!(&src, Value::Obj(o) if matches!(
                                    o.borrow().kind,
                                    ObjKind::Arr(_)
                                        | ObjKind::Iter { .. }
                                        | ObjKind::IterLazy(_)
                                ));
                            if internal {
                                convert(to_elems(i, &src)?, i)?
                            } else {
                                // spec InitializeTypedArrayFromArrayLike:
                                // LengthOfArrayLike feeds
                                // AllocateTypedArrayBuffer — a length beyond
                                // the host element cap is a RangeError before
                                // any element is read (`to_elems` would
                                // silently clamp). Each Get then interleaves
                                // with its ToNumber/ToBigInt — a valueOf on
                                // element k may mutate element k+1.
                                let len =
                                    i.get_prop(&src, &PropKey::from_str("length"))?;
                                let n = i.to_num(&len)?;
                                if n.is_nan() || n <= 0.0 {
                                    Vec::new()
                                } else if !n.is_finite() || n > 8_000_000.0 {
                                    return Err(
                                        i.range_err("Invalid typed array length")
                                    );
                                } else {
                                    let n = n.trunc() as usize;
                                    let mut elems =
                                        Vec::with_capacity(n.min(1_000_000));
                                    for k in 0..n {
                                        let it = i.get_prop(&src, &ta_elem_key(k))?;
                                        elems.push(ta_elem_num(i, &it, target_big)?);
                                    }
                                    elems
                                }
                            }
                        } else {
                            if !i.is_callable(&iterm) {
                                return Err(i.type_err("value is not iterable"));
                            }
                            convert(i.iter_values(&src)?, i)?
                        };
                    let (buf, n) = mk_buf_of(i, &elems, kind);
                    (buf, 0, n, false)
                }
            }
        }
        Value::Und | Value::Null => (mk_buffer(i, Vec::new()), 0, 0, false),
        other => {
            // spec InitializeTypedArrayFromLength — ToIndex semantics:
            // NaN → +0, fractions truncate, negatives / ±∞ / ≥ 2^53 are
            // RangeErrors. The host limit applies to the BYTE count —
            // `n * sz` must not overflow or exceed the data-block cap.
            let n = to_index(i, other)?;
            if n > AB_HOST_MAX / sz {
                return Err(i.range_err("Invalid typed array length"));
            }
            (mk_buffer(i, vec![0u8; n * sz]), 0, n, false)
        }
    };
    let o = match this {
        Value::Obj(o) => o.clone(),
        _ => plain(global_ctor_proto(i, kind.type_name())),
    };
    o.borrow_mut().kind = ObjKind::Typed {
        buf,
        off,
        len,
        kind,
        track,
    };
    Ok(Value::Obj(o))
}

/// `%TypedArray%.of` — `new this(len)` then per-index Set.
fn ta_static_of(i: &mut Interp, t: &Value, args: &[Value]) -> NR {
    if !i.is_constructor(t) {
        return Err(i.type_err("of called on a non-constructor"));
    }
    let obj = i.construct(t, vec![Value::Num(args.len() as f64)], None)?;
    // spec TypedArrayCreateFromConstructor — the single-argument length
    // check plus ValidateTypedArray.
    let Some(tp) = ta_parts(&obj) else {
        return Err(i.type_err("of result is not a TypedArray"));
    };
    if tp.eff().is_none() {
        return Err(i.type_err("of result is a detached or out-of-bounds TypedArray"));
    }
    if tp.eff().unwrap_or(0) < args.len() {
        return Err(i.type_err("of result is too short"));
    }
    for (k, v) in args.iter().enumerate() {
        i.set_prop(&obj, ta_elem_key(k), v.clone())?;
    }
    Ok(obj)
}

/// `%TypedArray%.from` — iterable and array-like forms, with map fn.
fn ta_static_from(i: &mut Interp, t: &Value, args: &[Value]) -> NR {
    if !i.is_constructor(t) {
        return Err(i.type_err("from called on a non-constructor"));
    }
    let src = arg(args, 0);
    let mapf = arg(args, 1);
    let this_arg = arg(args, 2);
    if !matches!(mapf, Value::Und) && !i.is_callable(&mapf) {
        return Err(i.type_err("TypedArray.from mapper is not callable"));
    }
    let use_map = i.is_callable(&mapf);
    let iterm = i.get_prop(&src, &PropKey::Sym(SYM_ITERATOR))?;
    if !matches!(iterm, Value::Und | Value::Null) {
        // iterable source — drain via the next() protocol
        if !i.is_callable(&iterm) {
            return Err(i.type_err("value is not iterable"));
        }
        let iter = i.call(&iterm, src.clone(), Vec::new())?;
        let mut items = Vec::new();
        loop {
            if items.len() > 200_000 {
                return Err(i.range_err("iteration limit exceeded"));
            }
            let next = i.get_prop_str(&iter, "next")?;
            if !i.is_callable(&next) {
                return Err(i.type_err("iterator.next is not a function"));
            }
            let r = i.call(&next, iter.clone(), Vec::new())?;
            if !matches!(r, Value::Obj(_)) {
                return Err(i.type_err("iterator result is not an object"));
            }
            let done = i.get_prop_str(&r, "done")?;
            if i.to_bool(&done) {
                break;
            }
            items.push(i.get_prop_str(&r, "value")?);
        }
        let obj = i.construct(t, vec![Value::Num(items.len() as f64)], None)?;
        if ta_parts(&obj).is_none() {
            return Err(i.type_err("from result is not a TypedArray"));
        }
        for (k, it) in items.into_iter().enumerate() {
            let v = if use_map {
                i.call(&mapf, this_arg.clone(), vec![it, Value::Num(k as f64)])?
            } else {
                it
            };
            i.set_prop(&obj, ta_elem_key(k), v)?;
        }
        return Ok(obj);
    }
    // array-like source — length read, construct, then per-index
    // Get → map → Set (all errors propagate in order)
    let lv = i.get_prop_str(&src, "length")?;
    let n = i.to_num(&lv)?;
    let len = if n.is_nan() || n <= 0.0 {
        0
    } else {
        n.min(9_007_199_254_740_991.0) as usize
    };
    let obj = i.construct(t, vec![Value::Num(len as f64)], None)?;
    if ta_parts(&obj).is_none() {
        return Err(i.type_err("from result is not a TypedArray"));
    }
    let big = ta_is_big(&obj);
    for k in 0..len {
        let kv = i.get_prop(&src, &ta_elem_key(k))?;
        let v = if use_map {
            i.call(&mapf, this_arg.clone(), vec![kv, Value::Num(k as f64)])?
        } else {
            kv
        };
        i.set_prop(&obj, ta_elem_key(k), v)?;
    }
    Ok(obj)
}

/// the proto for results of methods on `this` (its own proto) — used
/// by the non-TypedArray installers below (Blob, media stubs).
fn this_proto(this: &Value) -> Option<ObjRef> {
    if let Value::Obj(o) = this {
        return o.borrow().proto.clone();
    }
    None
}

/// spec GetPrototypeFromConstructor — the observable
/// `Get(newTarget, "prototype")` replayed inside native constructors.
/// `Interp::construct` resolved the proto without invoking getters, so
/// the Get is performed here at its spec position: an Object result
/// becomes the receiver's [[Prototype]]; a non-object keeps the
/// already-chosen fallback (the callee's own `.prototype`).
fn apply_new_target_proto(i: &mut Interp, this: &Value) -> Result<(), Value> {
    if let Some(nt) = i.new_target_native.clone() {
        let pv = i.get_prop(&nt, &PropKey::from_str("prototype"))?;
        if let (Value::Obj(np), Value::Obj(o)) = (&pv, this) {
            o.borrow_mut().proto = Some(np.clone());
        }
    }
    Ok(())
}

/// spec ValidateTypedArray — receiver must be a TypedArray with a
/// non-detached, in-bounds buffer.
fn need_ta(i: &mut Interp, this: &Value) -> Result<Tap, Value> {
    let t = ta_parts(this).ok_or_else(|| {
        i.type_err("Method called on incompatible receiver (expected TypedArray)")
    })?;
    if t.eff().is_none() {
        return Err(i.type_err("Cannot perform operation on a detached or out-of-bounds TypedArray"));
    }
    Ok(t)
}

/// spec ValidateTypedArray in write mode — additionally rejects buffers
/// marked immutable *before* any argument coercion runs.
fn need_ta_write(i: &mut Interp, this: &Value) -> Result<Tap, Value> {
    let t = need_ta(i, this)?;
    if t.buf.borrow().immutable {
        return Err(i.type_err("Cannot write to an immutable ArrayBuffer"));
    }
    Ok(t)
}

fn m_ta_set(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    // spec order: RequireInternalSlot → immutable check → offset
    // coercion → detached / out-of-bounds check → source handling.
    // (immutable throws before ANY coercion; detached throws only
    // after the offset's valueOf has run.)
    let t = ta_parts(this).ok_or_else(|| {
        i.type_err("Method called on incompatible receiver (expected TypedArray)")
    })?;
    if t.buf.borrow().immutable {
        return Err(i.type_err("Cannot write to an immutable ArrayBuffer"));
    }
    let tof = ta_to_int(i, &arg(args, 1))?;
    // spec: a negative or +∞ offset is a RangeError, checked before the
    // detached / out-of-bounds validation inside the sub-ops.
    if tof < 0.0 || tof == f64::INFINITY {
        return Err(i.range_err("offset is out of bounds"));
    }
    let to = tof as usize;
    let src = arg(args, 0);
    let target_big = ta_is_big(this);
    if let Some(s) = ta_parts(&src) {
        // TypedArray source — spec SetTypedArrayFromTypedArray:
        // out-of-bounds checks on both records, the ContentType check,
        // the length range check, then the copy.
        let Some(tlen) = t.eff() else {
            return Err(
                i.type_err("Cannot perform set on a detached or out-of-bounds TypedArray")
            );
        };
        let Some(slen) = s.eff() else {
            return Err(i.type_err("source TypedArray is detached or out of bounds"));
        };
        // spec: the ContentType check precedes the length range check —
        // a mixed bigint/number set is a TypeError even when the source
        // would also be too long.
        if ta_is_big(&src) != target_big {
            return Err(i.type_err("Cannot mix BigInt and non-BigInt TypedArrays"));
        }
        if to.saturating_add(slen) > tlen {
            return Err(i.range_err("source array is too long"));
        }
        let elems = ta_f64s(&src);
        if let Some(bytes) = buffer_bytes(&t.buf) {
            let mut bb = bytes.borrow_mut();
            let sz = t.kind.size();
            for (k, v) in elems.iter().enumerate() {
                t.kind.write(&mut bb, t.off + (to + k) * sz, *v);
            }
        }
        return Ok(Value::Und);
    }
    // array-like / primitive source — spec SetTypedArrayFromArrayLike:
    // ValidateTypedArray(target) and targetLength come BEFORE
    // ToObject(source) / LengthOfArrayLike(source), so length getters
    // observe the original bounds while writes stay live-bounded.
    let Some(tlen) = t.eff() else {
        return Err(
            i.type_err("Cannot perform set on a detached or out-of-bounds TypedArray")
        );
    };
    if matches!(src, Value::Und | Value::Null) {
        return Err(i.type_err("set source must be an object"));
    }
    let lv = i.get_prop_str(&src, "length")?;
    let n = i.to_num(&lv)?;
    let slen = if n.is_nan() || n <= 0.0 {
        0
    } else {
        n.min(9_007_199_254_740_991.0) as usize
    };
    if to.saturating_add(slen) > tlen {
        return Err(i.range_err("source array is too long"));
    }
    for k in 0..slen {
        // ? Get — an abrupt completion leaves earlier writes in place.
        let v = i.get_prop(&src, &ta_elem_key(k))?;
        // spec Set propagates the element conversion first —
        // conversion side effects (detach/resize) make the write
        // itself silently vanish.
        let nv = ta_elem_num(i, &v, target_big)?;
        t.set_num(i, to + k, nv)?;
    }
    Ok(Value::Und)
}

fn m_ta_subarray(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    // spec requires only the internal-slot check — begin/end still get
    // coerced when the buffer is detached or out-of-bounds, and any
    // failure surfaces from the species construction below.
    let t = ta_parts(this).ok_or_else(|| {
        i.type_err("Method called on incompatible receiver (expected TypedArray)")
    })?;
    // srcLength = TypedArrayLength(record): live length for tracking
    // views, the stored length for in-bounds fixed views, and 0 when the
    // view is detached or out of bounds (a shrunk resizable buffer can
    // put a fixed view out of bounds without throwing here — the
    // species construction below surfaces any RangeError).
    let len = t.eff().unwrap_or(0);
    let begin = clamp_index(ta_to_int(i, &arg(args, 0))?, len);
    let end = match arg(args, 1) {
        Value::Und => len,
        v => clamp_index(ta_to_int(i, &v)?, len),
    };
    let nlen = end.saturating_sub(begin);
    let noff = t.off + begin * t.kind.size();
    // species ctor gets (buffer, byteOffset[, length]); the two-arg
    // form preserves length-tracking when `this` tracks and `end`
    // was not given.
    let argv = if t.track && matches!(arg(args, 1), Value::Und) {
        vec![Value::Obj(t.buf.clone()), Value::Num(noff as f64)]
    } else {
        vec![
            Value::Obj(t.buf.clone()),
            Value::Num(noff as f64),
            Value::Num(nlen as f64),
        ]
    };
    ta_species_result(i, this, t.kind, argv, None)
}

fn m_ta_slice(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let t = need_ta(i, this)?;
    let len = t.eff().unwrap_or(0);
    let begin = clamp_index(ta_to_int(i, &arg(args, 0))?, len);
    let end = match arg(args, 1) {
        Value::Und => len,
        v => clamp_index(ta_to_int(i, &v)?, len),
    };
    let count = end.saturating_sub(begin);
    let a = ta_species_result(
        i,
        this,
        t.kind,
        vec![Value::Num(count as f64)],
        Some(count),
    )?;
    // spec re-validates the source after species construction — a
    // detached or shrunk-out-of-bounds view throws even when count is 0.
    if t.eff().is_none() {
        return Err(
            i.type_err("Cannot perform slice on a detached or out-of-bounds ArrayBuffer")
        );
    }
    if count > 0 {
        // spec: same element kind → byte-level copy (requires the same
        // [[ContentType]] too — bigint-kind storage shares TAK::F64
        // with Float64Array). Source bytes past the live (possibly
        // shrunk) buffer read as zero — the fresh destination buffer
        // is already zero-initialised.
        let src_big = ta_is_big(this);
        let a_big = ta_is_big(&a);
        if a_big != src_big {
            // per-element Set would fail on the first conversion —
            // ToNumber(bigint) / ToBigInt(number) both throw
            return Err(i.type_err("Cannot mix BigInt and non-BigInt TypedArrays"));
        }
        if ta_parts(&a).is_some_and(|d| d.kind == t.kind) {
            let d = ta_parts(&a).unwrap();
            let sz = t.kind.size();
            let src_at = t.off + begin * sz;
            if let (Some(sb), Some(db)) = (buffer_bytes(&t.buf), buffer_bytes(&d.buf)) {
                let avail = sb.borrow().len().saturating_sub(src_at);
                let n_bytes = (count * sz)
                    .min(avail)
                    .min(db.borrow().len().saturating_sub(d.off));
                let tmp: Vec<u8> = sb.borrow()[src_at..src_at + n_bytes].to_vec();
                db.borrow_mut()[d.off..d.off + n_bytes].copy_from_slice(&tmp);
            }
        } else {
            // different element kinds — per-element Get → Set; a custom
            // species result may alias this buffer so snapshot first.
            let mut vals = Vec::with_capacity(count);
            for k in 0..count {
                vals.push(ta_elem(i, this, begin + k)?);
            }
            for (k, v) in vals.into_iter().enumerate() {
                i.set_prop(&a, ta_elem_key(k), v)?;
            }
        }
    }
    Ok(a)
}

fn m_ta_fill(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let t = need_ta_write(i, this)?;
    let len = t.eff().unwrap_or(0);
    // spec: ToBigInt on bigint-kind receivers, ToNumber otherwise
    let v = ta_elem_num(i, &arg(args, 0), ta_is_big(this))?;
    let start = clamp_index(ta_to_int(i, &arg(args, 1))?, len);
    let end = match arg(args, 2) {
        Value::Und => len,
        x => clamp_index(ta_to_int(i, &x)?, len),
    };
    // coercion may have detached the buffer or shrunk it out of bounds
    if t.eff().is_none() {
        return Err(i.type_err("Cannot perform fill on a detached or out-of-bounds TypedArray"));
    }
    for k in start..end {
        t.set_num(i, k, v)?;
    }
    Ok(this.clone())
}

fn m_ta_copy_within(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let t = need_ta_write(i, this)?;
    let len = t.eff().unwrap_or(0);
    let target = clamp_index(ta_to_int(i, &arg(args, 0))?, len);
    let start = clamp_index(ta_to_int(i, &arg(args, 1))?, len);
    let end = match arg(args, 2) {
        Value::Und => len,
        x => clamp_index(ta_to_int(i, &x)?, len),
    };
    let count = end.saturating_sub(start).min(len - target);
    if count > 0 {
        // spec re-validates after coercion — detachment or an
        // out-of-bounds view throws.
        if t.eff().is_none() {
            return Err(
                i.type_err("Cannot perform copyWithin on a detached or out-of-bounds TypedArray")
            );
        }
        // byte-level copy against the LIVE buffer — a shrunk source
        // range contributes only the still-present bytes.
        let sz = t.kind.size();
        let Some(b) = buffer_bytes(&t.buf) else {
            return Ok(this.clone());
        };
        let mut bb = b.borrow_mut();
        let blen = bb.len();
        let to_byte = (t.off + target * sz).min(blen);
        let from_byte = (t.off + start * sz).min(blen);
        let count_bytes = (count * sz)
            .min(blen - to_byte)
            .min(blen - from_byte);
        let tmp: Vec<u8> = bb[from_byte..from_byte + count_bytes].to_vec();
        bb[to_byte..to_byte + count_bytes].copy_from_slice(&tmp);
    }
    Ok(this.clone())
}

fn m_ta_reverse(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let t = need_ta_write(i, this)?;
    let _ = args;
    let len = t.eff().unwrap_or(0);
    let tmp = ta_f64s(this);
    for k in 0..len {
        t.set_num(i, k, tmp[len - 1 - k])?;
    }
    Ok(this.clone())
}

/// shared element capture + sort + write-back for sort/toSorted.
fn ta_sort_elems(
    i: &mut Interp,
    this: &Value,
    len: usize,
    cmp: &Value,
) -> Result<Vec<Value>, Value> {
    let mut items = Vec::with_capacity(len);
    for k in 0..len {
        items.push(ta_elem(i, this, k)?);
    }
    if i.is_callable(cmp) {
        // bigint-kind arrays hand stand-ins to the comparator so user
        // code observes bigint-typed elements
        let big = ta_is_big(this);
        let mut err: Option<Value> = None;
        let mut work = items.clone();
        merge_sort(&mut items, &mut work, &mut |a, b| {
            if err.is_some() {
                return std::cmp::Ordering::Equal;
            }
            let (av, bv) = if big {
                (
                    match a {
                        Value::Num(n) => i.bigint_obj(*n),
                        v => v.clone(),
                    },
                    match b {
                        Value::Num(n) => i.bigint_obj(*n),
                        v => v.clone(),
                    },
                )
            } else {
                (a.clone(), b.clone())
            };
            match i.call(cmp, Value::Und, vec![av, bv]) {
                Ok(v) => match i.to_num(&v) {
                    Ok(n) if n < 0.0 => std::cmp::Ordering::Less,
                    Ok(n) if n > 0.0 => std::cmp::Ordering::Greater,
                    Ok(_) => std::cmp::Ordering::Equal,
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
        // default numeric compare; NaN (incl. undefined elements) last
        items.sort_by(|a, b| {
            let (x, y) = match (a, b) {
                (Value::Num(x), Value::Num(y)) => (*x, *y),
                (Value::Num(_), _) => return std::cmp::Ordering::Less,
                (_, Value::Num(_)) => return std::cmp::Ordering::Greater,
                _ => (f64::NAN, f64::NAN),
            };
            if x.is_nan() && y.is_nan() {
                std::cmp::Ordering::Equal
            } else if x.is_nan() {
                std::cmp::Ordering::Greater
            } else if y.is_nan() {
                std::cmp::Ordering::Less
            } else {
                x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal)
            }
        });
    }
    Ok(items)
}

fn m_ta_sort(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    // spec checks comparefn callability before ValidateTypedArray
    let cmp = arg(args, 0);
    if !matches!(cmp, Value::Und) && !i.is_callable(&cmp) {
        return Err(i.type_err("comparefn must be a function or undefined"));
    }
    let t = need_ta_write(i, this)?;
    let len = t.eff().unwrap_or(0);
    let items = ta_sort_elems(i, this, len, &cmp)?;
    // write-back via Set — live bounds; immutable throws TypeError
    for (k, v) in items.into_iter().enumerate() {
        i.set_prop(this, ta_elem_key(k), v)?;
    }
    Ok(this.clone())
}

fn m_ta_index_of(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let t = need_ta(i, this)?;
    let len = t.eff().unwrap_or(0);
    if len == 0 {
        return Ok(Value::Num(-1.0));
    }
    let needle = arg(args, 0);
    let n = match arg(args, 1) {
        Value::Und => 0.0,
        v => ta_to_int(i, &v)?,
    };
    if n == f64::INFINITY {
        return Ok(Value::Num(-1.0));
    }
    let big = ta_is_big(this);
    let start = if n < 0.0 {
        len.saturating_sub((-n) as usize)
    } else {
        (n as usize).min(len)
    };
    for k in start..len {
        let v = ta_elem(i, this, k)?;
        if ta_elem_matches(&v, &needle, big, false) {
            return Ok(Value::Num(k as f64));
        }
    }
    Ok(Value::Num(-1.0))
}

fn m_ta_last_index_of(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let t = need_ta(i, this)?;
    let len = t.eff().unwrap_or(0);
    if len == 0 {
        return Ok(Value::Num(-1.0));
    }
    let needle = arg(args, 0);
    let n = match arg(args, 1) {
        Value::Und => len as f64 - 1.0,
        v => ta_to_int(i, &v)?,
    };
    if n == f64::NEG_INFINITY {
        return Ok(Value::Num(-1.0));
    }
    let k: i64 = if n >= 0.0 {
        n.min(len as f64 - 1.0) as i64
    } else {
        len as i64 + n as i64
    };
    if k < 0 {
        return Ok(Value::Num(-1.0));
    }
    let big = ta_is_big(this);
    for k in (0..=k as usize).rev() {
        let v = ta_elem(i, this, k)?;
        if ta_elem_matches(&v, &needle, big, false) {
            return Ok(Value::Num(k as f64));
        }
    }
    Ok(Value::Num(-1.0))
}

fn m_ta_includes(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let t = need_ta(i, this)?;
    let len = t.eff().unwrap_or(0);
    if len == 0 {
        return Ok(Value::Bool(false));
    }
    let needle = arg(args, 0);
    let n = match arg(args, 1) {
        Value::Und => 0.0,
        v => ta_to_int(i, &v)?,
    };
    if n == f64::INFINITY {
        return Ok(Value::Bool(false));
    }
    let big = ta_is_big(this);
    let start = if n < 0.0 {
        len.saturating_sub((-n) as usize)
    } else {
        (n as usize).min(len)
    };
    for k in start..len {
        let v = ta_elem(i, this, k)?;
        if ta_elem_matches(&v, &needle, big, true) {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

fn m_ta_join(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let t = need_ta(i, this)?;
    let len = t.eff().unwrap_or(0);
    let sep = match arg(args, 0) {
        Value::Und => ",".to_string(),
        v => {
            if let Value::Sym(..) = v {
                return Err(i.type_err("Cannot convert a Symbol value to a string"));
            }
            i.to_str(&v)?
        }
    };
    let mut parts = Vec::with_capacity(len);
    for k in 0..len {
        let v = ta_elem(i, this, k)?;
        parts.push(match v {
            Value::Und | Value::Null => String::new(),
            v => i.to_str(&v)?,
        });
    }
    Ok(Value::Str(Rc::from(parts.join(&sep).as_str())))
}

fn m_ta_to_locale_string(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let _ = args;
    let t = need_ta(i, this)?;
    let len = t.eff().unwrap_or(0);
    let mut parts = Vec::with_capacity(len);
    for k in 0..len {
        let v = ta_elem(i, this, k)?;
        let s = match v {
            Value::Und | Value::Null => String::new(),
            v => {
                let m = i.get_prop_str(&v, "toLocaleString")?;
                if !i.is_callable(&m) {
                    return Err(i.type_err("element.toLocaleString is not callable"));
                }
                let r = i.call(&m, v.clone(), Vec::new())?;
                i.to_str(&r)?
            }
        };
        parts.push(s);
    }
    Ok(Value::Str(Rc::from(parts.join(",").as_str())))
}

fn m_ta_at(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let t = need_ta(i, this)?;
    let len = t.eff().unwrap_or(0);
    let n = ta_to_int(i, &arg(args, 0))?;
    let idx = if n < 0.0 {
        len as i64 + n as i64
    } else {
        n.min(i64::MAX as f64) as i64
    };
    if idx < 0 {
        return Ok(Value::Und);
    }
    // spec checks IsValidIntegerIndex against LIVE bounds — coercion
    // may have resized the buffer, so defer to the integer-indexed
    // [[Get]] (out-of-bounds reads as undefined either way).
    ta_pub_elem(i, this, idx as usize)
}

/// spec callback-preamble for map/filter/forEach/find*/some/every:
/// callable check happens before any iteration (throws even for
/// empty receivers).
fn ta_cb(i: &mut Interp, args: &[Value]) -> Result<(Value, Value), Value> {
    let cb = arg(args, 0);
    if !i.is_callable(&cb) {
        return Err(i.type_err("callback is not a function"));
    }
    Ok((cb, arg(args, 1)))
}

fn m_ta_map(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let t = need_ta(i, this)?;
    let len = t.eff().unwrap_or(0);
    let (cb, this_arg) = ta_cb(i, args)?;
    let a = ta_species_result(
        i,
        this,
        t.kind,
        vec![Value::Num(len as f64)],
        Some(len),
    )?;
    let a_big = ta_is_big(&a);
    for k in 0..len {
        let kv = ta_pub_elem(i, this, k)?;
        let rv = i.call(
            &cb,
            this_arg.clone(),
            vec![kv, Value::Num(k as f64), this.clone()],
        )?;
        // the Set inside the spec coerces with ToNumber/ToBigInt —
        // doing it here stores a plain number without double valueOf
        let cv = Value::Num(ta_elem_num(i, &rv, a_big)?);
        i.set_prop(&a, ta_elem_key(k), cv)?;
    }
    Ok(a)
}

fn m_ta_filter(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let t = need_ta(i, this)?;
    let len = t.eff().unwrap_or(0);
    let (cb, this_arg) = ta_cb(i, args)?;
    // callbacks run before the species create (spec order)
    let mut kept = Vec::new();
    for k in 0..len {
        let kv = ta_elem(i, this, k)?;
        let pv = ta_pubify(i, this, kv.clone());
        let r = i.call(
            &cb,
            this_arg.clone(),
            vec![pv, Value::Num(k as f64), this.clone()],
        )?;
        if i.to_bool(&r) {
            kept.push(kv);
        }
    }
    let a = ta_species_result(
        i,
        this,
        t.kind,
        vec![Value::Num(kept.len() as f64)],
        Some(kept.len()),
    )?;
    for (k, kv) in kept.into_iter().enumerate() {
        i.set_prop(&a, ta_elem_key(k), kv)?;
    }
    Ok(a)
}

fn m_ta_for_each(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let t = need_ta(i, this)?;
    let len = t.eff().unwrap_or(0);
    let (cb, this_arg) = ta_cb(i, args)?;
    for k in 0..len {
        let kv = ta_pub_elem(i, this, k)?;
        i.call(
            &cb,
            this_arg.clone(),
            vec![kv, Value::Num(k as f64), this.clone()],
        )?;
    }
    Ok(Value::Und)
}

fn m_ta_reduce(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let t = need_ta(i, this)?;
    let len = t.eff().unwrap_or(0);
    let cb = arg(args, 0);
    if !i.is_callable(&cb) {
        return Err(i.type_err("callback is not a function"));
    }
    let mut k = 0;
    let mut acc = match args.get(1) {
        Some(v) => v.clone(),
        None => {
            if len == 0 {
                return Err(i.type_err("Reduce of empty array with no initial value"));
            }
            k = 1;
            ta_pub_elem(i, this, 0)?
        }
    };
    while k < len {
        let kv = ta_pub_elem(i, this, k)?;
        acc = i.call(
            &cb,
            Value::Und,
            vec![acc, kv, Value::Num(k as f64), this.clone()],
        )?;
        k += 1;
    }
    Ok(acc)
}

fn m_ta_reduce_right(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let t = need_ta(i, this)?;
    let len = t.eff().unwrap_or(0);
    let cb = arg(args, 0);
    if !i.is_callable(&cb) {
        return Err(i.type_err("callback is not a function"));
    }
    let mut k = len;
    let mut acc = match args.get(1) {
        Some(v) => v.clone(),
        None => {
            if len == 0 {
                return Err(i.type_err("Reduce of empty array with no initial value"));
            }
            k = len - 1;
            ta_pub_elem(i, this, len - 1)?
        }
    };
    while k > 0 {
        k -= 1;
        let kv = ta_pub_elem(i, this, k)?;
        acc = i.call(
            &cb,
            Value::Und,
            vec![acc, kv, Value::Num(k as f64), this.clone()],
        )?;
    }
    Ok(acc)
}

fn m_ta_find(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let t = need_ta(i, this)?;
    let len = t.eff().unwrap_or(0);
    let (cb, this_arg) = ta_cb(i, args)?;
    for k in 0..len {
        let kv = ta_pub_elem(i, this, k)?;
        let r = i.call(
            &cb,
            this_arg.clone(),
            vec![kv.clone(), Value::Num(k as f64), this.clone()],
        )?;
        if i.to_bool(&r) {
            return Ok(kv);
        }
    }
    Ok(Value::Und)
}

fn m_ta_find_index(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let t = need_ta(i, this)?;
    let len = t.eff().unwrap_or(0);
    let (cb, this_arg) = ta_cb(i, args)?;
    for k in 0..len {
        let kv = ta_pub_elem(i, this, k)?;
        let r = i.call(
            &cb,
            this_arg.clone(),
            vec![kv, Value::Num(k as f64), this.clone()],
        )?;
        if i.to_bool(&r) {
            return Ok(Value::Num(k as f64));
        }
    }
    Ok(Value::Num(-1.0))
}

fn m_ta_find_last(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let t = need_ta(i, this)?;
    let len = t.eff().unwrap_or(0);
    let (cb, this_arg) = ta_cb(i, args)?;
    for k in (0..len).rev() {
        let kv = ta_pub_elem(i, this, k)?;
        let r = i.call(
            &cb,
            this_arg.clone(),
            vec![kv.clone(), Value::Num(k as f64), this.clone()],
        )?;
        if i.to_bool(&r) {
            return Ok(kv);
        }
    }
    Ok(Value::Und)
}

fn m_ta_find_last_index(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let t = need_ta(i, this)?;
    let len = t.eff().unwrap_or(0);
    let (cb, this_arg) = ta_cb(i, args)?;
    for k in (0..len).rev() {
        let kv = ta_pub_elem(i, this, k)?;
        let r = i.call(
            &cb,
            this_arg.clone(),
            vec![kv, Value::Num(k as f64), this.clone()],
        )?;
        if i.to_bool(&r) {
            return Ok(Value::Num(k as f64));
        }
    }
    Ok(Value::Num(-1.0))
}

fn m_ta_some(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let t = need_ta(i, this)?;
    let len = t.eff().unwrap_or(0);
    let (cb, this_arg) = ta_cb(i, args)?;
    for k in 0..len {
        let kv = ta_pub_elem(i, this, k)?;
        let r = i.call(
            &cb,
            this_arg.clone(),
            vec![kv, Value::Num(k as f64), this.clone()],
        )?;
        if i.to_bool(&r) {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

fn m_ta_every(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let t = need_ta(i, this)?;
    let len = t.eff().unwrap_or(0);
    let (cb, this_arg) = ta_cb(i, args)?;
    for k in 0..len {
        let kv = ta_pub_elem(i, this, k)?;
        let r = i.call(
            &cb,
            this_arg.clone(),
            vec![kv, Value::Num(k as f64), this.clone()],
        )?;
        if !i.to_bool(&r) {
            return Ok(Value::Bool(false));
        }
    }
    Ok(Value::Bool(true))
}

/// `{value, done}` iteration-result object.
fn iter_result(i: &mut Interp, v: Value, done: bool) -> NR {
    let r = i.new_obj();
    i.set_data(&r, "value", v);
    i.set_data(&r, "done", Value::Bool(done));
    Ok(Value::Obj(r))
}

/// `%TypedArray Iterator%.prototype` — per-interp singleton stashed
/// on `Object.prototype` (a hidden, non-enumerable slot). Inherits
/// the array-iterator proto for `@@iterator`; its own `next` and
/// `@@toStringTag` make it a distinct intrinsic as the spec requires.
fn ta_iter_proto(i: &mut Interp) -> ObjRef {
    let op = i.p.obj.clone();
    if let Some(Value::Obj(p)) =
        i.obj_get_raw(&op, &PropKey::from_str("__taIterProto"))
    {
        return p;
    }
    let p = plain(Some(i.p.array_iter.clone()));
    i.set_builtin(&p, "next", i.native("next", ta_iter_next));
    p.borrow_mut().props.insert(
        PropKey::Sym(SYM_TOSTRINGTAG),
        Prop::builtin(Value::Str(Rc::from("Typed Array Iterator"))),
    );
    op.borrow_mut()
        .props
        .insert_str("__taIterProto", Prop::builtin(Value::Obj(p.clone())));
    p
}

/// TypedArray iterator — reads the live view on every `next()` so
/// resizable-buffer length changes are observed dynamically.
/// `mode`: 0 = keys, 1 = values, 2 = entries.
fn mk_ta_iter(i: &mut Interp, this: &Value, mode: u8) -> Value {
    let it = plain(Some(ta_iter_proto(i)));
    {
        let mut b = it.borrow_mut();
        b.props.insert_str("__src", Prop::builtin(this.clone()));
        b.props.insert_str("__i", Prop::builtin(Value::Num(0.0)));
        b.props.insert_str("__m", Prop::builtin(Value::Num(mode as f64)));
    }
    Value::Obj(it)
}

fn ta_iter_next(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Value::Obj(it) = this else {
        return Err(i.type_err("not a TypedArray iterator"));
    };
    // an exhausted iterator stays done forever — spec clears the
    // iterated-object slot, so a subsequent out-of-bounds/detach does
    // not make later next() calls throw.
    let (src, k, mode) = {
        let b = it.borrow();
        if b.props.contains_str("__done") {
            return iter_result(i, Value::Und, true);
        }
        let src = b
            .props
            .get_str("__src")
            .map(|p| p.val.clone())
            .unwrap_or(Value::Und);
        let k = match b.props.get_str("__i").map(|p| p.val.clone()) {
            Some(Value::Num(n)) => n as usize,
            _ => 0,
        };
        let mode = match b.props.get_str("__m").map(|p| p.val.clone()) {
            Some(Value::Num(n)) => n as u8,
            _ => 1,
        };
        (src, k, mode)
    };
    let Some(t) = ta_parts(&src) else {
        return Err(i.type_err("not a TypedArray iterator"));
    };
    let Some(eff) = t.eff() else {
        return Err(i.type_err("Cannot iterate a detached or out-of-bounds TypedArray"));
    };
    if k >= eff {
        it.borrow_mut()
            .props
            .insert_str("__done", Prop::builtin(Value::Bool(true)));
        return iter_result(i, Value::Und, true);
    }
    if let Some(p) = it.borrow_mut().props.get_str_mut("__i") {
        p.val = Value::Num(k as f64 + 1.0);
    }
    let v = match mode {
        0 => Value::Num(k as f64),
        2 => {
            let e = ta_pub_elem(i, &src, k)?;
            Value::Obj(i.new_arr(vec![Value::Num(k as f64), e]))
        }
        _ => ta_pub_elem(i, &src, k)?,
    };
    iter_result(i, v, false)
}

fn m_ta_keys(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    need_ta(i, this)?;
    Ok(mk_ta_iter(i, this, 0))
}

fn m_ta_values(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    need_ta(i, this)?;
    Ok(mk_ta_iter(i, this, 1))
}

fn m_ta_entries(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    need_ta(i, this)?;
    Ok(mk_ta_iter(i, this, 2))
}

fn m_ta_to_reversed(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let _ = args;
    let t = need_ta(i, this)?;
    let len = t.eff().unwrap_or(0);
    let a = ta_fresh_typed(i, this, t.kind, len)?;
    for k in 0..len {
        let v = ta_elem(i, this, len - 1 - k)?;
        i.set_prop(&a, ta_elem_key(k), v)?;
    }
    Ok(a)
}

fn m_ta_to_sorted(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    // spec: comparefn callability is checked before ValidateTypedArray
    let cmp = arg(args, 0);
    if !matches!(cmp, Value::Und) && !i.is_callable(&cmp) {
        return Err(i.type_err("comparefn must be a function or undefined"));
    }
    let t = need_ta(i, this)?;
    let len = t.eff().unwrap_or(0);
    let items = ta_sort_elems(i, this, len, &cmp)?;
    let a = ta_fresh_typed(i, this, t.kind, len)?;
    for (k, v) in items.into_iter().enumerate() {
        i.set_prop(&a, ta_elem_key(k), v)?;
    }
    Ok(a)
}

fn m_ta_with(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let t = need_ta(i, this)?;
    let len = t.eff().unwrap_or(0);
    let n = ta_to_int(i, &arg(args, 0))?;
    let actual = if n < 0.0 {
        len as i64 + n as i64
    } else {
        n.min(i64::MAX as f64) as i64
    };
    // value conversion runs before the index validity check (a
    // valueOf may resize the buffer and invalidate the index); spec
    // uses ToBigInt on bigint-kind receivers, ToNumber otherwise
    let v = ta_elem_num(i, &arg(args, 1), ta_is_big(this))?;
    let Some(eff) = t.eff() else {
        return Err(i.range_err("Invalid index"));
    };
    if actual < 0 || actual >= eff as i64 {
        return Err(i.range_err("Invalid index"));
    }
    let actual = actual as usize;
    let a = ta_fresh_typed(i, this, t.kind, len)?;
    for k in 0..len {
        let kv = if k == actual {
            Value::Num(v)
        } else {
            ta_elem(i, this, k)?
        };
        i.set_prop(&a, ta_elem_key(k), kv)?;
    }
    Ok(a)
}

// ===========================================================================
// ArrayBuffer
// ===========================================================================

fn install_array_buffer(i: &mut Interp) {
    let p = i.p.arraybuffer.clone();
    met_a(i, &p, "slice", m_ab_slice, 2.0);
    met_a(i, &p, "sliceToImmutable", m_ab_slice_to_immutable, 2.0);
    met_a(i, &p, "resize", m_ab_resize, 1.0);
    met_a(i, &p, "transfer", m_ab_transfer, 0.0);
    met_a(i, &p, "transferToFixedLength", m_ab_transfer_fixed, 0.0);
    met_a(i, &p, "transferToImmutable", m_ab_transfer_immutable, 0.0);
    acc(i, &p, "byteLength", Some(ab_get_byte_len), None);
    acc(i, &p, "detached", Some(ab_get_detached), None);
    acc(i, &p, "maxByteLength", Some(ab_get_max_byte_len), None);
    acc(i, &p, "resizable", Some(ab_get_resizable), None);
    acc(i, &p, "immutable", Some(ab_get_immutable), None);
    p.borrow_mut().props.insert(
        PropKey::Sym(SYM_TOSTRINGTAG),
        Prop {
            val: Value::Str(Rc::from("ArrayBuffer")),
            writable: false,
            enumerable: false,
            configurable: true,
            get: None,
            set: None,
        },
    );
    let ctor = mk_ctor_len(i, "ArrayBuffer", ab_ctor, &p.clone(), 1.0);
    if let Value::Obj(co) = &ctor {
        met_a(i, co, "isView", ab_is_view, 1.0);
        i.set_builtin(co, "maxByteLength", Value::Num(268435456.0));
        // get %ArrayBuffer%[Symbol.species] — returns `this`
        let sp = i.native("get [Symbol.species]", |_i, t, _a| Ok(t.clone()));
        co.borrow_mut()
            .props
            .insert(PropKey::Sym(SYM_SPECIES), Prop::accessor(Some(sp), None));
    }
    gdef(i, "ArrayBuffer", ctor.clone());
    // SharedArrayBuffer — a distinct constructor whose instances are
    // marked `__shared`; every %ArrayBuffer.prototype% member rejects
    // them (the spec forbids shared receivers there).
    let sp = plain(Some(i.p.obj.clone()));
    met_a(i, &sp, "slice", m_sab_slice, 2.0);
    acc(i, &sp, "byteLength", Some(sab_get_byte_len), None);
    acc(i, &sp, "maxByteLength", Some(sab_get_max_byte_len), None);
    acc(i, &sp, "resizable", Some(sab_get_resizable), None);
    acc(i, &sp, "growable", Some(sab_get_growable), None);
    acc(i, &sp, "immutable", Some(sab_get_immutable), None);
    acc(i, &sp, "detached", Some(sab_get_detached), None);
    sp.borrow_mut().props.insert(
        PropKey::Sym(SYM_TOSTRINGTAG),
        Prop::builtin(Value::Str(Rc::from("SharedArrayBuffer"))),
    );
    let sctor = mk_ctor_len(i, "SharedArrayBuffer", sab_ctor, &sp, 1.0);
    gdef(i, "SharedArrayBuffer", sctor);
}

/// host limit on a real ArrayBuffer allocation — the spec allows the
/// host to fail a CreateByteDataBlock at its own discretion.
const AB_HOST_MAX: usize = 268435456;

fn ab_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    // `new_target_native` stays set while a native ctor body runs — a
    // bare `ArrayBuffer(...)` nested inside another constructor's
    // observable work must still throw. The freshly allocated Ordinary
    // `this` is what marks a real [[Construct]].
    let fresh_this = matches!(this, Value::Obj(o)
        if matches!(o.borrow().kind, ObjKind::Ord) && o.borrow().props.is_empty());
    if !(i.new_target_native.is_some() && fresh_this) {
        return Err(i.type_err("Constructor ArrayBuffer requires 'new'"));
    }
    // spec: byteLength is ToIndex'd — NaN → +0, negatives and
    // out-of-range values are RangeErrors.
    let n = match arg(args, 0) {
        Value::Und => 0,
        v => to_index(i, &v)?,
    };
    // options.maxByteLength — Get runs in order after the length
    // coercion; only an Object `options` is consulted per spec.
    let max = match arg(args, 1) {
        Value::Obj(opts) => {
            let v =
                i.get_prop(&Value::Obj(opts.clone()), &PropKey::from_str("maxByteLength"))?;
            if matches!(v, Value::Und) {
                None
            } else {
                Some(to_index(i, &v)?)
            }
        }
        _ => None,
    };
    // spec AllocateArrayBuffer step 3: byteLength > maxByteLength is a
    // RangeError *before* OrdinaryCreateFromConstructor — i.e. before
    // any observable Get(newTarget, "prototype").
    if max.is_some_and(|m| n > m) {
        return Err(i.range_err("maxByteLength is smaller than byteLength"));
    }
    // spec step 4: OrdinaryCreateFromConstructor — `construct` resolved
    // the proto without invoking getters, so replay the observable
    // Get(newTarget, "prototype") at its spec position (an Object
    // result becomes the receiver's proto).
    apply_new_target_proto(i, this)?;
    // spec step 5: CreateByteDataBlock — the host allocation limit is a
    // RangeError that only fires once object creation (with its proto
    // access) succeeded.
    if n > AB_HOST_MAX || max.is_some_and(|m| m > AB_HOST_MAX) {
        return Err(i.range_err("Invalid array buffer length"));
    }
    let o = match this {
        Value::Obj(o) => o.clone(),
        _ => plain(Some(i.p.arraybuffer.clone())),
    };
    o.borrow_mut().rab_max = max;
    o.borrow_mut().kind = ObjKind::Bytes(Rc::new(RefCell::new(vec![0u8; n])));
    Ok(Value::Obj(o))
}

/// `true` for SharedArrayBuffer instances — %ArrayBuffer.prototype%
/// members must reject them.
fn is_shared(o: &ObjRef) -> bool {
    o.borrow().props.contains_str("__shared")
}

fn m_ab_slice(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Value::Obj(o) = this else {
        return Err(i.type_err("slice called on non-ArrayBuffer"));
    };
    if buffer_bytes(o).is_none() || is_shared(o) {
        return Err(i.type_err("slice called on non-ArrayBuffer"));
    }
    if o.borrow().detached {
        return Err(i.type_err("Cannot perform slice on a detached ArrayBuffer"));
    }
    let len = buffer_bytes(o).unwrap().borrow().len();
    let start = clamp_index(ta_to_int(i, &arg(args, 0))?, len);
    let end = match arg(args, 1) {
        Value::Und => len,
        v => clamp_index(ta_to_int(i, &v)?, len),
    };
    let new_len = end.saturating_sub(start);
    // SpeciesConstructor(O, %ArrayBuffer%)
    let ctor = match i.get_prop_str(this, "constructor")? {
        Value::Und => None,
        c @ Value::Obj(_) => {
            let s = i.get_prop(&c, &PropKey::Sym(SYM_SPECIES))?;
            match s {
                Value::Und | Value::Null => None,
                s if i.is_constructor(&s) => Some(s),
                _ => {
                    return Err(
                        i.type_err("constructor[Symbol.species] is not a constructor")
                    )
                }
            }
        }
        _ => return Err(i.type_err("constructor is not an object")),
    };
    let nv = match ctor {
        Some(c) => i.construct(&c, vec![Value::Num(new_len as f64)], None)?,
        None => {
            let g = i.global.clone();
            let cv = i
                .env_lookup(&g, "ArrayBuffer")?
                .unwrap_or(Value::Und);
            i.construct(&cv, vec![Value::Num(new_len as f64)], None)?
        }
    };
    // result validation
    let Value::Obj(no) = &nv else {
        return Err(i.type_err("slice result is not an ArrayBuffer"));
    };
    if is_shared(no) || buffer_bytes(no).is_none() {
        return Err(i.type_err("slice result is not an ArrayBuffer"));
    }
    if Rc::ptr_eq(no, o) {
        return Err(i.type_err("slice result must not be the same ArrayBuffer"));
    }
    if no.borrow().detached {
        return Err(i.type_err("slice result is a detached ArrayBuffer"));
    }
    if buffer_bytes(no).unwrap().borrow().len() < new_len {
        return Err(i.type_err("slice result is too small"));
    }
    // a user species constructor may have detached the source
    if o.borrow().detached {
        return Err(i.type_err("Cannot perform slice on a detached ArrayBuffer"));
    }
    if no.borrow().immutable {
        return Err(i.type_err("slice result is an immutable ArrayBuffer"));
    }
    let sb = buffer_bytes(o).unwrap();
    let db = buffer_bytes(no).unwrap();
    // spec re-reads the source length: side effects may have shrunk it,
    // in which case only the still-present bytes are copied (the fresh
    // destination tail is already zero).
    let sb_len = sb.borrow().len();
    let copy_len = new_len.min(sb_len.saturating_sub(start));
    if copy_len > 0 {
        db.borrow_mut()[..copy_len]
            .copy_from_slice(&sb.borrow()[start..start + copy_len]);
    }
    Ok(nv)
}

fn need_ab(i: &mut Interp, this: &Value) -> Result<(ObjRef, Rc<RefCell<Vec<u8>>>), Value> {
    let Value::Obj(o) = this else {
        return Err(i.type_err("Method called on incompatible receiver (expected ArrayBuffer)"));
    };
    if is_shared(o) {
        return Err(i.type_err("operation not permitted on a SharedArrayBuffer"));
    }
    let Some(b) = buffer_bytes(o) else {
        return Err(i.type_err("Method called on incompatible receiver (expected ArrayBuffer)"));
    };
    Ok((o.clone(), b))
}

fn ab_get_byte_len(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let (o, b) = need_ab(i, this)?;
    Ok(Value::Num(if o.borrow().detached {
        0.0
    } else {
        b.borrow().len() as f64
    }))
}

fn ab_get_detached(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let (o, _) = need_ab(i, this)?;
    let d = o.borrow().detached;
    Ok(Value::Bool(d))
}

fn ab_get_max_byte_len(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let (o, b) = need_ab(i, this)?;
    let v = if o.borrow().detached {
        0.0
    } else if let Some(m) = o.borrow().rab_max {
        m as f64
    } else {
        b.borrow().len() as f64
    };
    Ok(Value::Num(v))
}

fn ab_get_resizable(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let (o, _) = need_ab(i, this)?;
    let r = o.borrow().rab_max.is_some();
    Ok(Value::Bool(r))
}

fn ab_get_immutable(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let (o, _) = need_ab(i, this)?;
    let im = o.borrow().immutable;
    Ok(Value::Bool(im))
}

fn m_ab_resize(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (o, b) = need_ab(i, this)?;
    // internal-slot validations run before the argument coercion —
    // immutable buffers also cannot be resized at all.
    let Some(max) = o.borrow().rab_max else {
        return Err(i.type_err("ArrayBuffer is not resizable"));
    };
    if o.borrow().immutable {
        return Err(i.type_err("Cannot resize an immutable ArrayBuffer"));
    }
    // the coercion itself may detach the buffer, so the detached
    // check comes only after ToIndex runs.
    let n = to_index(i, &arg(args, 0))?;
    if o.borrow().detached {
        return Err(i.type_err("Cannot resize a detached ArrayBuffer"));
    }
    if n > max || n > AB_HOST_MAX {
        return Err(i.range_err("new length is out of bounds"));
    }
    b.borrow_mut().resize(n, 0);
    Ok(Value::Und)
}

/// `mode`: 0 = `transfer` (resizability preserved), 1 =
/// `transferToFixedLength`, 2 = `transferToImmutable`.
fn ab_transfer_into(i: &mut Interp, this: &Value, args: &[Value], mode: u8) -> NR {
    let (o, b) = need_ab(i, this)?;
    // spec ArrayBufferCopyAndDetach: the newLength coercion runs
    // BEFORE the detached / immutable checks — a valueOf must still
    // fire on a doomed receiver.
    let old_len = b.borrow().len();
    let new_len = match arg(args, 0) {
        Value::Und => old_len,
        v => to_index(i, &v)?,
    };
    if o.borrow().detached {
        return Err(i.type_err("Cannot transfer a detached ArrayBuffer"));
    }
    if o.borrow().immutable {
        return Err(i.type_err("Cannot transfer an immutable ArrayBuffer"));
    }
    if new_len > AB_HOST_MAX {
        return Err(i.range_err("new length is out of bounds"));
    }
    let max = o.borrow().rab_max;
    let mut data = std::mem::take(&mut *b.borrow_mut());
    o.borrow_mut().detached = true;
    data.resize(new_len, 0);
    data.truncate(new_len);
    let nb = mk_buffer(i, data);
    match mode {
        // spec: preserve-resizability keeps a resizable buffer resizable
        // with newMax = max(newByteLength, oldMax); the other modes
        // produce fixed-length (or immutable fixed-length) buffers.
        0 => nb.borrow_mut().rab_max = max.map(|m| m.max(new_len)),
        2 => nb.borrow_mut().immutable = true,
        _ => {}
    }
    Ok(Value::Obj(nb))
}

fn m_ab_transfer(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    ab_transfer_into(i, this, args, 0)
}

fn m_ab_transfer_fixed(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    ab_transfer_into(i, this, args, 1)
}

fn m_ab_transfer_immutable(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    ab_transfer_into(i, this, args, 2)
}

fn m_ab_slice_to_immutable(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let v = m_ab_slice(i, this, args)?;
    if let Value::Obj(o) = &v {
        o.borrow_mut().immutable = true;
    }
    Ok(v)
}

fn ab_is_view(_i: &mut Interp, _t: &Value, args: &[Value]) -> NR {
    let r = matches!(&arg(args, 0), Value::Obj(o)
        if matches!(o.borrow().kind, ObjKind::Typed { .. } | ObjKind::View { .. }));
    Ok(Value::Bool(r))
}

// -- SharedArrayBuffer --------------------------------------------------------

/// `new SharedArrayBuffer(length[, options])` — byte storage marked
/// `__shared` so %ArrayBuffer.prototype% members reject it.
fn sab_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let fresh_this = matches!(this, Value::Obj(o)
        if matches!(o.borrow().kind, ObjKind::Ord) && o.borrow().props.is_empty());
    if !(i.new_target_native.is_some() && fresh_this) {
        return Err(i.type_err("Constructor SharedArrayBuffer requires 'new'"));
    }
    let n = match arg(args, 0) {
        Value::Und => 0,
        v => to_index(i, &v)?,
    };
    // growable SharedArrayBuffers — options.maxByteLength
    let max = match arg(args, 1) {
        Value::Obj(opts) => {
            let v =
                i.get_prop(&Value::Obj(opts.clone()), &PropKey::from_str("maxByteLength"))?;
            if matches!(v, Value::Und) {
                None
            } else {
                Some(to_index(i, &v)?)
            }
        }
        _ => None,
    };
    // spec AllocateSharedArrayBuffer: byteLength > maxByteLength is a
    // RangeError before OrdinaryCreateFromConstructor.
    if max.is_some_and(|m| n > m) {
        return Err(i.range_err("maxByteLength is smaller than byteLength"));
    }
    // OrdinaryCreateFromConstructor — `construct` resolved the proto
    // without invoking getters, so replay the observable
    // Get(newTarget, "prototype") at its spec position (an Object
    // result becomes the receiver's proto).
    apply_new_target_proto(i, this)?;
    // CreateSharedByteDataBlock — host allocation limit (RangeError)
    // fires only after the proto access above succeeded.
    if n > AB_HOST_MAX || max.is_some_and(|m| m > AB_HOST_MAX) {
        return Err(i.range_err("Invalid array buffer length"));
    }
    let o = match this {
        Value::Obj(o) => o.clone(),
        _ => plain(global_ctor_proto(i, "SharedArrayBuffer")),
    };
    o.borrow_mut().rab_max = max;
    o.borrow_mut().kind = ObjKind::Bytes(Rc::new(RefCell::new(vec![0u8; n])));
    o.borrow_mut()
        .props
        .insert_str("__shared", Prop::builtin(Value::Bool(true)));
    Ok(Value::Obj(o))
}

/// spec RequireInternalSlot — a %SharedArrayBuffer.prototype% member
/// only accepts objects carrying the `__shared` marker.
fn need_sab(i: &mut Interp, this: &Value) -> Result<(ObjRef, Rc<RefCell<Vec<u8>>>), Value> {
    let Value::Obj(o) = this else {
        return Err(i.type_err("Method called on incompatible receiver (expected SharedArrayBuffer)"));
    };
    if !is_shared(o) {
        return Err(i.type_err("Method called on incompatible receiver (expected SharedArrayBuffer)"));
    }
    let Some(b) = buffer_bytes(o) else {
        return Err(i.type_err("Method called on incompatible receiver (expected SharedArrayBuffer)"));
    };
    Ok((o.clone(), b))
}

fn sab_get_byte_len(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let (_, b) = need_sab(i, this)?;
    let n = b.borrow().len();
    Ok(Value::Num(n as f64))
}

fn sab_get_max_byte_len(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let (o, b) = need_sab(i, this)?;
    let n = o.borrow().rab_max.unwrap_or_else(|| b.borrow().len());
    Ok(Value::Num(n as f64))
}

fn sab_get_resizable(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let _ = need_sab(i, this)?;
    Ok(Value::Bool(false))
}

fn sab_get_growable(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let (o, _) = need_sab(i, this)?;
    let g = o.borrow().rab_max.is_some();
    Ok(Value::Bool(g))
}

fn sab_get_immutable(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let _ = need_sab(i, this)?;
    Ok(Value::Bool(false))
}

fn sab_get_detached(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let _ = need_sab(i, this)?;
    Ok(Value::Bool(false))
}

/// `SharedArrayBuffer.prototype.slice` — returns a *new shared* buffer.
fn m_sab_slice(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (_, b) = need_sab(i, this)?;
    let len = b.borrow().len();
    let start = clamp_index(ta_to_int(i, &arg(args, 0))?, len);
    let end = match arg(args, 1) {
        Value::Und => len,
        v => clamp_index(ta_to_int(i, &v)?, len),
    };
    let new_len = end.saturating_sub(start);
    let data = b.borrow()[start..start + new_len].to_vec();
    let nb = mk_buffer(i, data);
    nb.borrow_mut()
        .props
        .insert_str("__shared", Prop::builtin(Value::Bool(true)));
    // slice results keep the SharedArrayBuffer prototype
    if let Value::Obj(no) = this {
        if let Some(p) = global_ctor_proto(i, "SharedArrayBuffer") {
            nb.borrow_mut().proto = Some(p);
        } else {
            nb.borrow_mut().proto = no.borrow().proto.clone();
        }
    }
    Ok(Value::Obj(nb))
}

// ===========================================================================
// DataView
// ===========================================================================

fn install_data_view(i: &mut Interp) {
    let p = plain(Some(i.p.obj.clone()));
    for (n, f) in [
        ("getUint8", dv_get_u8 as NativeFn),
        ("getInt8", dv_get_i8),
        ("getUint16", dv_get_u16),
        ("getInt16", dv_get_i16),
        ("getUint32", dv_get_u32),
        ("getInt32", dv_get_i32),
        ("getFloat16", dv_get_f16),
        ("getFloat32", dv_get_f32),
        ("getFloat64", dv_get_f64),
        ("getBigInt64", dv_get_i64),
        ("getBigUint64", dv_get_u64),
        ("setUint8", dv_set_u8),
        ("setInt8", dv_set_u8),
        ("setUint16", dv_set_u16),
        ("setInt16", dv_set_u16),
        ("setUint32", dv_set_u32),
        ("setInt32", dv_set_u32),
        ("setFloat16", dv_set_f16),
        ("setFloat32", dv_set_f32),
        ("setFloat64", dv_set_f64),
        ("setBigInt64", dv_set_i64),
        ("setBigUint64", dv_set_u64),
    ] {
        let arity = if n.starts_with("get") { 1.0 } else { 2.0 };
        met_a(i, &p, n, f, arity);
    }
    acc(i, &p, "buffer", Some(dv_get_buffer), None);
    acc(i, &p, "byteLength", Some(dv_get_byte_len), None);
    acc(i, &p, "byteOffset", Some(dv_get_byte_off), None);
    p.borrow_mut().props.insert(
        PropKey::Sym(SYM_TOSTRINGTAG),
        Prop {
            val: Value::Str(Rc::from("DataView")),
            writable: false,
            enumerable: false,
            configurable: true,
            get: None,
            set: None,
        },
    );
    let ctor = mk_ctor_len(i, "DataView", dv_ctor, &p, 1.0);
    gdef(i, "DataView", ctor);
}

/// `ObjKind::View.len` sentinel for a length-tracking DataView on a
/// resizable buffer (`new DataView(rab)` with no byteLength argument).
const DV_AUTO: usize = usize::MAX;

/// spec GetViewByteLength — the live byte length of the view; `None`
/// when the buffer is detached or the view is out of bounds.
fn dv_state(buf: &ObjRef, off: usize, len: usize) -> Option<usize> {
    let b = buf.borrow();
    if b.detached {
        return None;
    }
    let ObjKind::Bytes(bytes) = &b.kind else {
        return None;
    };
    let blen = bytes.borrow().len();
    if off > blen {
        return None;
    }
    if len == DV_AUTO {
        Some(blen - off)
    } else if len > blen - off {
        None
    } else {
        Some(len)
    }
}

/// (bytes Rc, absolute byte offset, live byte length) — `None` for
/// non-views and detached / out-of-bounds views.
fn dv_parts(v: &Value) -> Option<(Rc<RefCell<Vec<u8>>>, usize, usize)> {
    if let Value::Obj(o) = v {
        if let ObjKind::View { buf, off, len } = &o.borrow().kind {
            let vlen = dv_state(buf, *off, *len)?;
            let b = buffer_bytes(buf)?;
            return Some((b, *off, vlen));
        }
    }
    None
}

fn need_view(i: &mut Interp, this: &Value) -> Result<(ObjRef, usize, usize), Value> {
    if let Value::Obj(o) = this {
        if let ObjKind::View { buf, off, len } = &o.borrow().kind {
            return Ok((buf.clone(), *off, *len));
        }
    }
    Err(i.type_err("Method called on incompatible receiver (expected DataView)"))
}

fn dv_get_buffer(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let (buf, _, _) = need_view(i, this)?;
    Ok(Value::Obj(buf))
}

fn dv_get_byte_len(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let (buf, off, len) = need_view(i, this)?;
    // detached / out-of-bounds views report 0
    Ok(Value::Num(dv_state(&buf, off, len).unwrap_or(0) as f64))
}

fn dv_get_byte_off(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let (buf, off, len) = need_view(i, this)?;
    Ok(Value::Num(if dv_state(&buf, off, len).is_some() {
        off as f64
    } else {
        0.0
    }))
}

fn dv_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let fresh_this = matches!(this, Value::Obj(o)
        if matches!(o.borrow().kind, ObjKind::Ord) && o.borrow().props.is_empty());
    if !(i.new_target_native.is_some() && fresh_this) {
        return Err(i.type_err("Constructor DataView requires 'new'"));
    }
    let Value::Obj(bo) = &arg(args, 0) else {
        return Err(i.type_err("First argument to DataView constructor must be an ArrayBuffer"));
    };
    let Some(b) = buffer_bytes(bo) else {
        return Err(i.type_err("First argument to DataView constructor must be an ArrayBuffer"));
    };
    // spec order: ToIndex(offset) → detached → offset bounds →
    // (explicit byteLength only) ToIndex(length) → bounds vs the
    // already-captured buffer length → proto access → detached →
    // bounds re-checks.
    let off = match arg(args, 1) {
        Value::Und => 0,
        v => to_index(i, &v)?,
    };
    if bo.borrow().detached {
        return Err(i.type_err("Cannot construct a DataView on a detached ArrayBuffer"));
    }
    let blen = b.borrow().len();
    if off > blen {
        return Err(i.range_err("Start offset is outside the bounds of the buffer"));
    }
    let resizable = bo.borrow().rab_max.is_some();
    let len = match arg(args, 2) {
        Value::Und if resizable => DV_AUTO,
        Value::Und => blen - off,
        v => {
            let l = to_index(i, &v)?;
            // spec: the bounds check uses bufferByteLength as captured
            // before the coercion — a detaching valueOf surfaces via the
            // post-proto-access check below instead.
            if l > blen - off {
                return Err(i.range_err("Invalid DataView length"));
            }
            l
        }
    };
    // spec OrdinaryCreateFromConstructor — `construct` resolved the
    // prototype without invoking getters, so replay the observable
    // Get(newTarget, "prototype") here (an Object result becomes the
    // receiver's proto): a getter may detach/resize the buffer, after
    // which the detached + bounds checks rerun.
    apply_new_target_proto(i, this)?;
    if bo.borrow().detached {
        return Err(i.type_err("Cannot construct a DataView on a detached ArrayBuffer"));
    }
    let blen = b.borrow().len();
    if off > blen {
        return Err(i.range_err("Start offset is outside the bounds of the buffer"));
    }
    if len != DV_AUTO && len > blen - off {
        return Err(i.range_err("Invalid DataView length"));
    }
    let o = match this {
        Value::Obj(o) => o.clone(),
        _ => plain(global_ctor_proto(i, "DataView")),
    };
    o.borrow_mut().kind = ObjKind::View {
        buf: bo.clone(),
        off,
        len,
    };
    Ok(Value::Obj(o))
}

/// spec GetViewValue — ToIndex first, then the detached / out-of-bounds
/// check, then the range check.
fn dv_get_index(
    i: &mut Interp,
    this: &Value,
    args: &[Value],
    sz: usize,
) -> Result<(Rc<RefCell<Vec<u8>>>, usize), Value> {
    let (buf, off, len) = need_view(i, this)?;
    let idx = to_index(i, &arg(args, 0))?;
    let Some(vlen) = dv_state(&buf, off, len) else {
        return Err(
            i.type_err("Cannot perform operation on a detached or out-of-bounds DataView")
        );
    };
    if idx > vlen || sz > vlen - idx {
        return Err(i.range_err("Offset is outside the bounds of the DataView"));
    }
    let b = buffer_bytes(&buf).unwrap();
    Ok((b, off + idx))
}

/// spec SetViewValue — the immutable check runs BEFORE any argument
/// coercion; then ToIndex, the value conversion (which may detach or
/// resize), bounds checks.
/// `big` selects ToBigInt for the BigInt64 setters, ToNumber else.
fn dv_set_index(
    i: &mut Interp,
    this: &Value,
    args: &[Value],
    sz: usize,
    big: bool,
) -> Result<(Rc<RefCell<Vec<u8>>>, usize, f64), Value> {
    let (buf, off, len) = need_view(i, this)?;
    if buf.borrow().immutable {
        return Err(i.type_err("Cannot write to an immutable ArrayBuffer"));
    }
    let idx = to_index(i, &arg(args, 0))?;
    let v = ta_elem_num(i, &arg(args, 1), big)?;
    let Some(vlen) = dv_state(&buf, off, len) else {
        return Err(
            i.type_err("Cannot perform operation on a detached or out-of-bounds DataView")
        );
    };
    if idx > vlen || sz > vlen - idx {
        return Err(i.range_err("Offset is outside the bounds of the DataView"));
    }
    let b = buffer_bytes(&buf).unwrap();
    Ok((b, off + idx, v))
}

fn dv_le(i: &mut Interp, args: &[Value], n: usize) -> bool {
    i.to_bool(&arg(args, n))
}

/// IEEE-754 binary16 → f64 (exact — every f16 is representable).
fn f16_bits_to_f64(h: u16) -> f64 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let e = (h >> 10) & 0x1f;
    let f = (h & 0x3ff) as f64;
    if e == 0 {
        sign * f * 5.9604644775390625e-8 // subnormals: f * 2^-24
    } else if e == 0x1f {
        if f == 0.0 {
            sign * f64::INFINITY
        } else {
            f64::NAN
        }
    } else {
        sign * (1.0 + f / 1024.0) * 2f64.powi(e as i32 - 15)
    }
}

/// f64 → IEEE-754 binary16 bits, round-to-nearest-even.
fn f64_to_f16_bits(v: f64) -> u16 {
    if v.is_nan() {
        return 0x7e00;
    }
    // f64 → f32 preserves order; the second rounding can't be
    // observed for values that land exactly on an f16 boundary.
    let bits = (v as f32).to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32;
    let man = bits & 0x007f_ffff;
    if exp == 0xff {
        return sign | if man == 0 { 0x7c00 } else { 0x7e00 };
    }
    let e16 = exp - 127 + 15;
    if e16 >= 0x1f {
        return sign | 0x7c00; // overflow → ±Inf
    }
    if e16 <= 0 {
        // subnormal / underflow — shift the implicit-1 mantissa down
        if e16 < -10 {
            return sign; // too small → ±0
        }
        let m = man | 0x0080_0000;
        let sh = (14 - e16) as u32;
        let rem = m & ((1u32 << sh) - 1);
        let half = 1u32 << (sh - 1);
        let mut out = m >> sh;
        if rem > half || (rem == half && out & 1 == 1) {
            out += 1;
        }
        return sign | out as u16;
    }
    // normal — round mantissa 23 → 10 bits
    let rem = man & 0x1fff;
    let mut m = (man >> 13) as u16;
    let mut e = e16 as u16;
    if rem > 0x1000 || (rem == 0x1000 && m & 1 == 1) {
        m += 1;
        if m == 0x400 {
            m = 0;
            e += 1;
        }
    }
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    sign | (e << 10) | m
}

fn dv_get_f16(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (b, at) = dv_get_index(i, this, args, 2)?;
    let bb = b.borrow();
    let mut s = [0u8; 2];
    s.copy_from_slice(&bb[at..at + 2]);
    let h = if dv_le(i, args, 1) {
        u16::from_le_bytes(s)
    } else {
        u16::from_be_bytes(s)
    };
    Ok(Value::Num(f16_bits_to_f64(h)))
}

fn dv_set_f16(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (b, at, v) = dv_set_index(i, this, args, 2, false)?;
    let h = f64_to_f16_bits(v);
    let s = if dv_le(i, args, 2) {
        h.to_le_bytes()
    } else {
        h.to_be_bytes()
    };
    b.borrow_mut()[at..at + 2].copy_from_slice(&s);
    Ok(Value::Und)
}

fn dv_get_u8(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (b, at) = dv_get_index(i, this, args, 1)?;
    let v = b.borrow()[at] as f64;
    Ok(Value::Num(v))
}
fn dv_get_i8(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (b, at) = dv_get_index(i, this, args, 1)?;
    let v = b.borrow()[at] as i8 as f64;
    Ok(Value::Num(v))
}
fn dv_get_u16(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (b, at) = dv_get_index(i, this, args, 2)?;
    let bb = b.borrow();
    let mut s = [0u8; 2];
    s.copy_from_slice(&bb[at..at + 2]);
    let v = if dv_le(i, args, 1) {
        u16::from_le_bytes(s)
    } else {
        u16::from_be_bytes(s)
    };
    Ok(Value::Num(v as f64))
}
fn dv_get_i16(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (b, at) = dv_get_index(i, this, args, 2)?;
    let bb = b.borrow();
    let mut s = [0u8; 2];
    s.copy_from_slice(&bb[at..at + 2]);
    let v = if dv_le(i, args, 1) {
        i16::from_le_bytes(s)
    } else {
        i16::from_be_bytes(s)
    };
    Ok(Value::Num(v as f64))
}
fn dv_get_u32(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (b, at) = dv_get_index(i, this, args, 4)?;
    let bb = b.borrow();
    let mut s = [0u8; 4];
    s.copy_from_slice(&bb[at..at + 4]);
    let v = if dv_le(i, args, 1) {
        u32::from_le_bytes(s)
    } else {
        u32::from_be_bytes(s)
    };
    Ok(Value::Num(v as f64))
}
fn dv_get_i32(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (b, at) = dv_get_index(i, this, args, 4)?;
    let bb = b.borrow();
    let mut s = [0u8; 4];
    s.copy_from_slice(&bb[at..at + 4]);
    let v = if dv_le(i, args, 1) {
        i32::from_le_bytes(s)
    } else {
        i32::from_be_bytes(s)
    };
    Ok(Value::Num(v as f64))
}
fn dv_get_f32(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (b, at) = dv_get_index(i, this, args, 4)?;
    let bb = b.borrow();
    let mut s = [0u8; 4];
    s.copy_from_slice(&bb[at..at + 4]);
    let v = if dv_le(i, args, 1) {
        f32::from_le_bytes(s)
    } else {
        f32::from_be_bytes(s)
    };
    Ok(Value::Num(v as f64))
}
fn dv_get_f64(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (b, at) = dv_get_index(i, this, args, 8)?;
    let bb = b.borrow();
    let mut s = [0u8; 8];
    s.copy_from_slice(&bb[at..at + 8]);
    let v = if dv_le(i, args, 1) {
        f64::from_le_bytes(s)
    } else {
        f64::from_be_bytes(s)
    };
    Ok(Value::Num(v))
}

/// BigInt element reads return the engine's bigint stand-in object so
/// `typeof`/`instanceof`-agnostic numeric consumers still work.
fn dv_get_i64(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (b, at) = dv_get_index(i, this, args, 8)?;
    let bb = b.borrow();
    let mut s = [0u8; 8];
    s.copy_from_slice(&bb[at..at + 8]);
    let v = if dv_le(i, args, 1) {
        i64::from_le_bytes(s)
    } else {
        i64::from_be_bytes(s)
    };
    Ok(i.bigint_obj(v as f64))
}
fn dv_get_u64(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (b, at) = dv_get_index(i, this, args, 8)?;
    let bb = b.borrow();
    let mut s = [0u8; 8];
    s.copy_from_slice(&bb[at..at + 8]);
    let v = if dv_le(i, args, 1) {
        u64::from_le_bytes(s)
    } else {
        u64::from_be_bytes(s)
    };
    Ok(i.bigint_obj(v as f64))
}
fn dv_set_u8(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (b, at, v) = dv_set_index(i, this, args, 1, false)?;
    b.borrow_mut()[at] = to_u32(v) as u8;
    Ok(Value::Und)
}
fn dv_set_u16(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (b, at, v) = dv_set_index(i, this, args, 2, false)?;
    let s = if dv_le(i, args, 2) {
        (to_u32(v) as u16).to_le_bytes()
    } else {
        (to_u32(v) as u16).to_be_bytes()
    };
    b.borrow_mut()[at..at + 2].copy_from_slice(&s);
    Ok(Value::Und)
}
fn dv_set_u32(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (b, at, v) = dv_set_index(i, this, args, 4, false)?;
    let s = if dv_le(i, args, 2) {
        to_u32(v).to_le_bytes()
    } else {
        to_u32(v).to_be_bytes()
    };
    b.borrow_mut()[at..at + 4].copy_from_slice(&s);
    Ok(Value::Und)
}
fn dv_set_f32(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (b, at, v) = dv_set_index(i, this, args, 4, false)?;
    let v = v as f32;
    let s = if dv_le(i, args, 2) {
        v.to_le_bytes()
    } else {
        v.to_be_bytes()
    };
    b.borrow_mut()[at..at + 4].copy_from_slice(&s);
    Ok(Value::Und)
}
fn dv_set_f64(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (b, at, v) = dv_set_index(i, this, args, 8, false)?;
    let s = if dv_le(i, args, 2) {
        v.to_le_bytes()
    } else {
        v.to_be_bytes()
    };
    b.borrow_mut()[at..at + 8].copy_from_slice(&s);
    Ok(Value::Und)
}
fn dv_set_i64(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (b, at, v) = dv_set_index(i, this, args, 8, true)?;
    let s = if dv_le(i, args, 2) {
        (v as i64).to_le_bytes()
    } else {
        (v as i64).to_be_bytes()
    };
    b.borrow_mut()[at..at + 8].copy_from_slice(&s);
    Ok(Value::Und)
}
fn dv_set_u64(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (b, at, v) = dv_set_index(i, this, args, 8, true)?;
    let s = if dv_le(i, args, 2) {
        (v as u64).to_le_bytes()
    } else {
        (v as u64).to_be_bytes()
    };
    b.borrow_mut()[at..at + 8].copy_from_slice(&s);
    Ok(Value::Und)
}

// ===========================================================================
// TextEncoder / TextDecoder
// ===========================================================================

fn install_text_codecs(i: &mut Interp) {
    {
        let p = plain(Some(i.p.obj.clone()));
        met(i, &p, "encode", te_encode);
        met(i, &p, "encodeInto", te_encode_into);
        let ctor = mk_ctor(i, "TextEncoder", te_ctor, &p);
        gdef(i, "TextEncoder", ctor);
    }
    {
        let p = plain(Some(i.p.obj.clone()));
        met(i, &p, "decode", td_decode);
        let ctor = mk_ctor(i, "TextDecoder", td_ctor, &p);
        gdef(i, "TextDecoder", ctor);
    }
}

fn te_ctor(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    if let Value::Obj(o) = this {
        i.set_builtin(o, "encoding", Value::Str(Rc::from("utf-8")));
    }
    Ok(match this {
        Value::Obj(_) => this.clone(),
        _ => {
            let o = plain(global_ctor_proto(i, "TextEncoder"));
            i.set_builtin(&o, "encoding", Value::Str(Rc::from("utf-8")));
            Value::Obj(o)
        }
    })
}

fn te_encode(i: &mut Interp, _t: &Value, args: &[Value]) -> NR {
    let s = i.to_str(&arg(args, 0))?;
    let data = s.into_bytes();
    let n = data.len();
    let buf = mk_buffer(i, data);
    Ok(Value::Obj(mk_typed(
        global_ctor_proto(i, "Uint8Array"),
        buf,
        0,
        n,
        TAK::U8,
        false,
    )))
}

fn te_encode_into(i: &mut Interp, _t: &Value, args: &[Value]) -> NR {
    let s = i.to_str(&arg(args, 0))?;
    let mut read = 0usize; // utf-16 code units consumed
    let mut written = 0usize;
    if let Some(tp) = ta_parts(&arg(args, 1)) {
        let cap = tp.eff().unwrap_or(0) * tp.kind.size();
        if let Some(bytes) = buffer_bytes(&tp.buf) {
            let mut bb = bytes.borrow_mut();
            for c in s.chars() {
                let mut tmp = [0u8; 4];
                let enc = c.encode_utf8(&mut tmp);
                if written + enc.len() > cap {
                    break;
                }
                bb[tp.off + written..tp.off + written + enc.len()]
                    .copy_from_slice(enc.as_bytes());
                written += enc.len();
                read += c.len_utf16();
            }
        }
    }
    let r = i.new_obj();
    i.set_data(&r, "read", Value::Num(read as f64));
    i.set_data(&r, "written", Value::Num(written as f64));
    Ok(Value::Obj(r))
}

fn td_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let label = i
        .to_str(&arg(args, 0))
        .unwrap_or_else(|_| "utf-8".to_string());
    let enc = if label.is_empty() {
        "utf-8".to_string()
    } else {
        label
    };
    let o = match this {
        Value::Obj(o) => o.clone(),
        _ => plain(global_ctor_proto(i, "TextDecoder")),
    };
    i.set_builtin(&o, "encoding", Value::Str(Rc::from(enc.as_str())));
    i.set_builtin(&o, "fatal", Value::Bool(false));
    i.set_builtin(&o, "ignoreBOM", Value::Bool(false));
    Ok(Value::Obj(o))
}

/// bytes behind a buffer-or-view value (None if it isn't one).
fn view_bytes(v: &Value) -> Option<Vec<u8>> {
    let Value::Obj(o) = v else { return None };
    match &o.borrow().kind {
        ObjKind::Bytes(b) => Some(b.borrow().clone()),
        ObjKind::Typed {
            buf,
            off,
            len,
            kind,
            track,
        } => {
            let b = buffer_bytes(buf)?;
            let bb = b.borrow();
            let start = (*off).min(bb.len());
            let eff = typed_state(buf, *off, *len, *kind, *track).unwrap_or(0);
            let end = (*off + eff * kind.size()).min(bb.len());
            Some(bb[start..end].to_vec())
        }
        ObjKind::View { buf, off, len } => {
            let b = buffer_bytes(buf)?;
            let bb = b.borrow();
            let start = (*off).min(bb.len());
            let vlen = dv_state(buf, *off, *len).unwrap_or(0);
            let end = (*off + vlen).min(bb.len());
            Some(bb[start..end].to_vec())
        }
        _ => None,
    }
}

fn td_decode(_i: &mut Interp, _t: &Value, args: &[Value]) -> NR {
    let data = match view_bytes(&arg(args, 0)) {
        Some(d) => d,
        None => Vec::new(),
    };
    Ok(Value::Str(Rc::from(
        String::from_utf8_lossy(&data).to_string().as_str(),
    )))
}

// ===========================================================================
// URL + URLSearchParams
// ===========================================================================

#[derive(Clone, Default)]
struct Url {
    scheme: String,
    user: String,
    pass: String,
    host: String,
    port: String,
    path: String,
    query: Option<String>,
    frag: Option<String>,
    /// opaque path (blob:, data:, custom schemes w/o //)
    opaque: bool,
}

fn url_scheme(s: &str) -> Option<(&str, &str)> {
    let b = s.as_bytes();
    if b.is_empty() || !b[0].is_ascii_alphabetic() {
        return None;
    }
    for (k, &c) in b.iter().enumerate().skip(1) {
        if c == b':' {
            return Some((&s[..k], &s[k + 1..]));
        }
        if !(c.is_ascii_alphanumeric() || c == b'+' || c == b'-' || c == b'.') {
            return None;
        }
    }
    None
}

fn url_special(scheme: &str) -> bool {
    matches!(scheme, "http" | "https" | "ws" | "wss" | "ftp" | "file")
}

fn url_default_port(scheme: &str) -> &'static str {
    match scheme {
        "http" | "ws" => "80",
        "https" | "wss" => "443",
        "ftp" => "21",
        _ => "\u{0}none",
    }
}

fn url_parse_authority(auth: &str, u: &mut Url) {
    // [user[:pass]@]host[:port]
    let hostport = match auth.rfind('@') {
        Some(at) => {
            let ui = &auth[..at];
            match ui.split_once(':') {
                Some((usr, pw)) => {
                    u.user = pct_decode(usr).unwrap_or_else(|| usr.to_string());
                    u.pass = pct_decode(pw).unwrap_or_else(|| pw.to_string());
                }
                None => u.user = pct_decode(ui).unwrap_or_else(|| ui.to_string()),
            }
            &auth[at + 1..]
        }
        None => auth,
    };
    // ipv6 [::1]:port
    if let Some(close) = hostport.find(']') {
        if hostport.starts_with('[') {
            u.host = hostport[..=close].to_string();
            if let Some(p) = hostport[close + 1..].strip_prefix(':') {
                u.port = p.to_string();
            }
            return;
        }
    }
    match hostport.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()) => {
            u.host = h.to_string();
            u.port = p.trim_start_matches('0').to_string();
            if u.port.is_empty() {
                u.port = "0".into();
            }
        }
        _ => u.host = hostport.to_string(),
    }
    u.host = u.host.to_lowercase();
}

/// normalize `.`/`..` segments (keep leading & trailing slashes).
fn url_norm_path(p: &str) -> String {
    if p.is_empty() {
        return String::new();
    }
    let mut segs: Vec<&str> = Vec::new();
    for (k, s) in p.split('/').enumerate() {
        if k == 0 && s.is_empty() {
            continue; // leading '/' is re-added below
        }
        match s {
            "." => {}
            ".." => {
                segs.pop();
            }
            _ => segs.push(s),
        }
    }
    let mut out = String::new();
    let leading = p.starts_with('/');
    if leading {
        out.push('/');
    }
    out.push_str(&segs.join("/"));
    if (p.ends_with("/.") || p.ends_with("/..") || p.ends_with('/')) && !out.ends_with('/') {
        out.push('/');
    }
    if out.is_empty() && leading {
        out.push('/');
    }
    out
}

/// merge a relative path onto the base's directory.
fn url_merge_path(base_path: &str, rel: &str) -> String {
    let dir = match base_path.rfind('/') {
        Some(at) => &base_path[..=at],
        None => "",
    };
    url_norm_path(&format!("{dir}{rel}"))
}

fn url_parse(input: &str, base: Option<&Url>) -> Option<Url> {
    let s = input.trim_matches(|c: char| c <= ' ' || c == '\u{7f}');
    if let Some((scheme, rest)) = url_scheme(s) {
        let scheme = scheme.to_lowercase();
        let mut u = Url {
            scheme: scheme.clone(),
            ..Default::default()
        };
        let mut rest = rest;
        let has_authority = if rest.starts_with("//") {
            rest = &rest[2..];
            true
        } else if url_special(&scheme) && rest.starts_with('/') {
            // `https:/x` — special scheme tolerates a single slash
            rest = &rest[1..];
            true
        } else {
            false
        };
        if has_authority {
            let end = rest
                .find(|c| matches!(c, '/' | '?' | '#'))
                .unwrap_or(rest.len());
            url_parse_authority(&rest[..end], &mut u);
            rest = &rest[end..];
        } else if !url_special(&scheme) {
            u.opaque = true;
        }
        url_parse_tail(rest, &mut u);
        if !u.opaque {
            u.path = url_norm_path(&u.path);
            if u.path.is_empty() && (!u.host.is_empty() || url_special(&scheme)) {
                u.path = "/".into();
            }
        }
        if u.port == url_default_port(&u.scheme) {
            u.port.clear();
        }
        Some(u)
    } else {
        let base = base?;
        if s.starts_with("//") {
            let mut u = Url {
                scheme: base.scheme.clone(),
                ..Default::default()
            };
            let rest = &s[2..];
            let end = rest
                .find(|c| matches!(c, '/' | '?' | '#'))
                .unwrap_or(rest.len());
            url_parse_authority(&rest[..end], &mut u);
            url_parse_tail(&rest[end..], &mut u);
            u.path = url_norm_path(&u.path);
            if u.path.is_empty() {
                u.path = "/".into();
            }
            if u.port == url_default_port(&u.scheme) {
                u.port.clear();
            }
            return Some(u);
        }
        // relative reference
        let rest = s;
        let mut u = base.clone();
        u.frag = None;
        if rest.starts_with('#') {
            u.frag = Some(pct_keep(&rest[1..]));
            return Some(u);
        }
        if rest.starts_with('?') {
            url_parse_qf(rest, &mut u);
            return Some(u);
        }
        if rest.is_empty() {
            return Some(u);
        }
        // path part up to ? or #
        let end = rest.find(|c| matches!(c, '?' | '#')).unwrap_or(rest.len());
        let p = &rest[..end];
        let rest = &rest[end..];
        if p.starts_with('/') {
            u.path = url_norm_path(p);
        } else {
            u.path = url_merge_path(&base.path, p);
            if u.path.is_empty() {
                u.path = "/".into();
            }
        }
        url_parse_qf(rest, &mut u);
        Some(u)
    }
}

/// parse `path?query#frag` — `rest` begins at the path or at '?'/'#'.
fn url_parse_tail(rest: &str, u: &mut Url) {
    let mut rest = rest;
    if let Some(at) = rest.find('#') {
        u.frag = Some(pct_keep(&rest[at + 1..]));
        rest = &rest[..at];
    }
    if let Some(at) = rest.find('?') {
        u.query = Some(pct_keep(&rest[at + 1..]));
        rest = &rest[..at];
    }
    u.path = rest.to_string();
}

/// parse only `?query#frag` (path untouched).
fn url_parse_qf(rest: &str, u: &mut Url) {
    let mut rest = rest;
    if let Some(at) = rest.find('#') {
        u.frag = Some(pct_keep(&rest[at + 1..]));
        rest = &rest[..at];
    }
    if let Some(q) = rest.strip_prefix('?') {
        u.query = Some(pct_keep(q));
    }
}

/// pass-through (URL parts stay percent-encoded; we don't re-encode).
fn pct_keep(s: &str) -> String {
    s.to_string()
}

fn pct_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
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
    String::from_utf8(out).ok()
}

fn url_serialize(u: &Url) -> String {
    let mut out = format!("{}:", u.scheme);
    if u.opaque {
        out.push_str(&u.path);
    } else {
        out.push_str("//");
        if !u.user.is_empty() || !u.pass.is_empty() {
            out.push_str(&u.user);
            if !u.pass.is_empty() {
                out.push(':');
                out.push_str(&u.pass);
            }
            out.push('@');
        }
        out.push_str(&u.host);
        if !u.port.is_empty() {
            out.push(':');
            out.push_str(&u.port);
        }
        out.push_str(&u.path);
    }
    if let Some(q) = &u.query {
        out.push('?');
        out.push_str(q);
    }
    if let Some(f) = &u.frag {
        out.push('#');
        out.push_str(f);
    }
    out
}

// hidden prop keys on URL instances
const U_SCHEME: &str = "\u{0}usc";
const U_USER: &str = "\u{0}uus";
const U_PASS: &str = "\u{0}upw";
const U_HOST: &str = "\u{0}uho";
const U_PORT: &str = "\u{0}upo";
const U_PATH: &str = "\u{0}upa";
const U_QUERY: &str = "\u{0}uqu";
const U_FRAG: &str = "\u{0}ufr";
const U_OPAQUE: &str = "\u{0}uop";
const U_SP: &str = "\u{0}usp";
const SP_PAIRS: &str = "\u{0}spp";
const SP_URL: &str = "\u{0}spu";

fn url_of(i: &Interp, o: &ObjRef) -> Url {
    let g = |k: &str| -> String {
        match i.obj_get_raw(o, &PropKey::from_str(k)) {
            Some(Value::Str(s)) => s.to_string(),
            _ => String::new(),
        }
    };
    Url {
        scheme: g(U_SCHEME),
        user: g(U_USER),
        pass: g(U_PASS),
        host: g(U_HOST),
        port: g(U_PORT),
        path: g(U_PATH),
        query: match i.obj_get_raw(o, &PropKey::from_str(U_QUERY)) {
            Some(Value::Str(s)) => Some(s.to_string()),
            _ => None,
        },
        frag: match i.obj_get_raw(o, &PropKey::from_str(U_FRAG)) {
            Some(Value::Str(s)) => Some(s.to_string()),
            _ => None,
        },
        opaque: matches!(
            i.obj_get_raw(o, &PropKey::from_str(U_OPAQUE)),
            Some(Value::Bool(true))
        ),
    }
}

fn url_store(i: &Interp, o: &ObjRef, u: &Url) {
    hid(i, o, U_SCHEME, Value::Str(Rc::from(u.scheme.as_str())));
    hid(i, o, U_USER, Value::Str(Rc::from(u.user.as_str())));
    hid(i, o, U_PASS, Value::Str(Rc::from(u.pass.as_str())));
    hid(i, o, U_HOST, Value::Str(Rc::from(u.host.as_str())));
    hid(i, o, U_PORT, Value::Str(Rc::from(u.port.as_str())));
    hid(i, o, U_PATH, Value::Str(Rc::from(u.path.as_str())));
    hid(
        i,
        o,
        U_QUERY,
        match &u.query {
            Some(q) => Value::Str(Rc::from(q.as_str())),
            None => Value::Und,
        },
    );
    hid(
        i,
        o,
        U_FRAG,
        match &u.frag {
            Some(f) => Value::Str(Rc::from(f.as_str())),
            None => Value::Und,
        },
    );
    hid(i, o, U_OPAQUE, Value::Bool(u.opaque));
}

fn install_url(i: &mut Interp) {
    // ---- URLSearchParams ----
    let sp_proto = plain(Some(i.p.obj.clone()));
    for (n, f) in [
        ("append", sp_append as NativeFn),
        ("delete", sp_delete),
        ("get", sp_get),
        ("getAll", sp_get_all),
        ("has", sp_has),
        ("set", sp_set),
        ("sort", sp_sort),
        ("toString", sp_to_string),
        ("forEach", sp_for_each),
        ("keys", sp_keys),
        ("values", sp_values),
        ("entries", sp_entries),
    ] {
        met(i, &sp_proto, n, f);
    }
    acc(i, &sp_proto, "size", Some(sp_size), None);
    let ent = i
        .obj_get_raw(&sp_proto, &PropKey::from_str("entries"))
        .unwrap();
    sp_proto
        .borrow_mut()
        .props
        .insert(PropKey::Sym(SYM_ITERATOR), Prop::builtin(ent));
    let sp_ctor = mk_ctor(i, "URLSearchParams", sp_ctor_f, &sp_proto);
    gdef(i, "URLSearchParams", sp_ctor);
    // stash the proto for URL.searchParams creation
    let gp = i.p.global.clone();
    hid(i, &gp, "\u{0}p_usp", Value::Obj(sp_proto));

    // ---- URL ----
    let up = plain(Some(i.p.obj.clone()));
    met(i, &up, "toString", url_href_get);
    met(i, &up, "toJSON", url_href_get);
    acc(i, &up, "href", Some(url_href_get), Some(url_href_set));
    acc(
        i,
        &up,
        "protocol",
        Some(url_protocol_get),
        Some(url_protocol_set),
    );
    acc(i, &up, "host", Some(url_host_get), Some(url_host_set));
    acc(
        i,
        &up,
        "hostname",
        Some(url_hostname_get),
        Some(url_hostname_set),
    );
    acc(i, &up, "port", Some(url_port_get), Some(url_port_set));
    acc(
        i,
        &up,
        "pathname",
        Some(url_pathname_get),
        Some(url_pathname_set),
    );
    acc(i, &up, "search", Some(url_search_get), Some(url_search_set));
    acc(i, &up, "hash", Some(url_hash_get), Some(url_hash_set));
    acc(i, &up, "origin", Some(url_origin_get), None);
    acc(
        i,
        &up,
        "username",
        Some(url_username_get),
        Some(url_username_set),
    );
    acc(
        i,
        &up,
        "password",
        Some(url_password_get),
        Some(url_password_set),
    );
    acc(i, &up, "searchParams", Some(url_search_params_get), None);
    let uctor = mk_ctor(i, "URL", url_ctor, &up);
    if let Value::Obj(co) = &uctor {
        i.set_builtin(
            co,
            "createObjectURL",
            i.native("createObjectURL", url_create_object_url),
        );
        i.set_builtin(
            co,
            "revokeObjectURL",
            i.native("revokeObjectURL", |_, _, _| Ok(Value::Und)),
        );
        i.set_builtin(co, "canParse", i.native("canParse", url_can_parse));
        i.set_builtin(co, "parse", i.native("parse", url_parse_static));
    }
    gdef(i, "URL", uctor);
}

fn url_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let href = i.to_str(&arg(args, 0))?;
    let base = match arg(args, 1) {
        Value::Und | Value::Null => None,
        v => {
            let bs = i.to_str(&v)?;
            url_parse(&bs, None)
        }
    };
    let Some(u) = url_parse(&href, base.as_ref()) else {
        return Err(i.type_err(format!("Invalid URL: {href}")));
    };
    let proto = match this {
        Value::Obj(o) => o.borrow().proto.clone(),
        _ => global_ctor_proto(i, "URL"),
    };
    let o = match this {
        Value::Obj(o) => o.clone(),
        _ => plain(proto),
    };
    url_store(i, &o, &u);
    Ok(Value::Obj(o))
}

fn this_url(i: &mut Interp, this: &Value) -> Result<(ObjRef, Url), Value> {
    let Value::Obj(o) = this else {
        return Err(i.type_err("URL method called on non-URL"));
    };
    Ok((o.clone(), url_of(i, o)))
}

fn url_href_get(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let (_, u) = this_url(i, this)?;
    Ok(Value::Str(Rc::from(url_serialize(&u).as_str())))
}

fn url_href_set(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (o, _) = this_url(i, this)?;
    let s = i.to_str(&arg(args, 0))?;
    if let Some(u) = url_parse(&s, None) {
        url_store(i, &o, &u);
        // drop cached searchParams pairs
        if let Some(Value::Obj(sp)) = i.obj_get_raw(&o, &PropKey::from_str(U_SP)) {
            sp_set_pairs(i, &sp, &sp_pairs_parse(u.query.as_deref().unwrap_or("")));
        }
    }
    Ok(Value::Und)
}

fn url_protocol_get(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let (_, u) = this_url(i, this)?;
    Ok(Value::Str(Rc::from(format!("{}:", u.scheme).as_str())))
}

fn url_protocol_set(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (o, mut u) = this_url(i, this)?;
    let s = i.to_str(&arg(args, 0))?;
    u.scheme = s.trim_end_matches(':').to_lowercase();
    url_store(i, &o, &u);
    Ok(Value::Und)
}

fn url_host_get(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let (_, u) = this_url(i, this)?;
    let h = if u.port.is_empty() {
        u.host.clone()
    } else {
        format!("{}:{}", u.host, u.port)
    };
    Ok(Value::Str(Rc::from(h.as_str())))
}

fn url_host_set(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (o, mut u) = this_url(i, this)?;
    let s = i.to_str(&arg(args, 0))?;
    let mut tmp = Url::default();
    url_parse_authority(&s, &mut tmp);
    u.host = tmp.host;
    u.port = tmp.port;
    if u.port == url_default_port(&u.scheme) {
        u.port.clear();
    }
    url_store(i, &o, &u);
    Ok(Value::Und)
}

fn url_hostname_get(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let (_, u) = this_url(i, this)?;
    Ok(Value::Str(Rc::from(u.host.as_str())))
}

fn url_hostname_set(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (o, mut u) = this_url(i, this)?;
    u.host = i.to_str(&arg(args, 0))?.to_lowercase();
    url_store(i, &o, &u);
    Ok(Value::Und)
}

fn url_port_get(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let (_, u) = this_url(i, this)?;
    Ok(Value::Str(Rc::from(u.port.as_str())))
}

fn url_port_set(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (o, mut u) = this_url(i, this)?;
    let s = i.to_str(&arg(args, 0))?;
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    u.port = digits.trim_start_matches('0').to_string();
    if u.port == url_default_port(&u.scheme) {
        u.port.clear();
    }
    url_store(i, &o, &u);
    Ok(Value::Und)
}

fn url_pathname_get(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let (_, u) = this_url(i, this)?;
    Ok(Value::Str(Rc::from(u.path.as_str())))
}

fn url_pathname_set(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (o, mut u) = this_url(i, this)?;
    if u.opaque {
        return Ok(Value::Und);
    }
    let s = i.to_str(&arg(args, 0))?;
    let owned = if s.starts_with('/') {
        s
    } else {
        format!("/{s}")
    };
    u.path = url_norm_path(&owned);
    url_store(i, &o, &u);
    Ok(Value::Und)
}

fn url_search_get(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let (_, u) = this_url(i, this)?;
    let s = match &u.query {
        Some(q) if !q.is_empty() => format!("?{q}"),
        _ => String::new(),
    };
    Ok(Value::Str(Rc::from(s.as_str())))
}

fn url_search_set(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (o, mut u) = this_url(i, this)?;
    let s = i.to_str(&arg(args, 0))?;
    let q = s.strip_prefix('?').unwrap_or(&s).to_string();
    u.query = if q.is_empty() { None } else { Some(q.clone()) };
    url_store(i, &o, &u);
    if let Some(Value::Obj(sp)) = i.obj_get_raw(&o, &PropKey::from_str(U_SP)) {
        sp_set_pairs(i, &sp, &sp_pairs_parse(&q));
    }
    Ok(Value::Und)
}

fn url_hash_get(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let (_, u) = this_url(i, this)?;
    let s = match &u.frag {
        Some(f) if !f.is_empty() => format!("#{f}"),
        _ => String::new(),
    };
    Ok(Value::Str(Rc::from(s.as_str())))
}

fn url_hash_set(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (o, mut u) = this_url(i, this)?;
    let s = i.to_str(&arg(args, 0))?;
    let f = s.strip_prefix('#').unwrap_or(&s).to_string();
    u.frag = if f.is_empty() { None } else { Some(f) };
    url_store(i, &o, &u);
    Ok(Value::Und)
}

fn url_origin_get(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let (_, u) = this_url(i, this)?;
    let o = if url_special(&u.scheme) && !u.host.is_empty() {
        let mut s = format!("{}://{}", u.scheme, u.host);
        if !u.port.is_empty() {
            s.push(':');
            s.push_str(&u.port);
        }
        s
    } else {
        "null".to_string()
    };
    Ok(Value::Str(Rc::from(o.as_str())))
}

fn url_username_get(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let (_, u) = this_url(i, this)?;
    Ok(Value::Str(Rc::from(u.user.as_str())))
}
fn url_username_set(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (o, mut u) = this_url(i, this)?;
    u.user = i.to_str(&arg(args, 0))?;
    url_store(i, &o, &u);
    Ok(Value::Und)
}
fn url_password_get(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let (_, u) = this_url(i, this)?;
    Ok(Value::Str(Rc::from(u.pass.as_str())))
}
fn url_password_set(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let (o, mut u) = this_url(i, this)?;
    u.pass = i.to_str(&arg(args, 0))?;
    url_store(i, &o, &u);
    Ok(Value::Und)
}

fn url_search_params_get(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let (o, u) = this_url(i, this)?;
    if let Some(v @ Value::Obj(_)) = i.obj_get_raw(&o, &PropKey::from_str(U_SP)) {
        return Ok(v);
    }
    let proto = i
        .obj_get_raw(&i.p.global.clone(), &PropKey::from_str("\u{0}p_usp"))
        .and_then(|v| if let Value::Obj(p) = v { Some(p) } else { None });
    let sp = plain(proto);
    hid(i, &sp, SP_URL, Value::Obj(o.clone()));
    sp_set_pairs(i, &sp, &sp_pairs_parse(u.query.as_deref().unwrap_or("")));
    hid(i, &o, U_SP, Value::Obj(sp.clone()));
    Ok(Value::Obj(sp))
}

fn url_create_object_url(i: &mut Interp, _t: &Value, _a: &[Value]) -> NR {
    i.sym_counter += 1;
    Ok(Value::Str(Rc::from(
        format!("blob:rowser-{:x}", i.sym_counter).as_str(),
    )))
}

fn url_can_parse(i: &mut Interp, _t: &Value, args: &[Value]) -> NR {
    let href = i.to_str(&arg(args, 0))?;
    let base = match arg(args, 1) {
        Value::Und | Value::Null => None,
        v => url_parse(&i.to_str(&v)?, None),
    };
    Ok(Value::Bool(url_parse(&href, base.as_ref()).is_some()))
}

fn url_parse_static(i: &mut Interp, _t: &Value, args: &[Value]) -> NR {
    let href = i.to_str(&arg(args, 0))?;
    let base = match arg(args, 1) {
        Value::Und | Value::Null => None,
        v => url_parse(&i.to_str(&v)?, None),
    };
    let Some(u) = url_parse(&href, base.as_ref()) else {
        return Ok(Value::Null);
    };
    let o = plain(global_ctor_proto(i, "URL"));
    url_store(i, &o, &u);
    Ok(Value::Obj(o))
}

// -- URLSearchParams ---------------------------------------------------------

fn sp_pairs_parse(q: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for part in q.split('&') {
        if part.is_empty() {
            continue;
        }
        let (k, v) = match part.split_once('=') {
            Some((k, v)) => (k, v),
            None => (part, ""),
        };
        let dec = |s: &str| -> String {
            let s = s.replace('+', " ");
            pct_decode(&s).unwrap_or(s)
        };
        out.push((dec(k), dec(v)));
    }
    out
}

fn sp_encode(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'*' | b'-' | b'.' | b'_') {
            out.push(b as char);
        } else if b == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn sp_serialize(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", sp_encode(k), sp_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn sp_get_pairs(i: &Interp, o: &ObjRef) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(Value::Obj(arr)) = i.obj_get_raw(o, &PropKey::from_str(SP_PAIRS)) {
        if let ObjKind::Arr(items) = &arr.borrow().kind {
            for it in items {
                if let Value::Obj(p) = it {
                    if let ObjKind::Arr(kv) = &p.borrow().kind {
                        if kv.len() >= 2 {
                            let k = match &kv[0] {
                                Value::Str(s) => s.to_string(),
                                _ => String::new(),
                            };
                            let v = match &kv[1] {
                                Value::Str(s) => s.to_string(),
                                _ => String::new(),
                            };
                            out.push((k, v));
                        }
                    }
                }
            }
        }
    }
    out
}

fn sp_set_pairs(i: &Interp, o: &ObjRef, pairs: &[(String, String)]) {
    let items: Vec<Value> = pairs
        .iter()
        .map(|(k, v)| {
            Value::Obj(i.new_arr(vec![
                Value::Str(Rc::from(k.as_str())),
                Value::Str(Rc::from(v.as_str())),
            ]))
        })
        .collect();
    let arr = i.new_arr(items);
    hid(i, o, SP_PAIRS, Value::Obj(arr));
}

/// after mutation, push serialized query back into the owner URL.
fn sp_sync(i: &Interp, o: &ObjRef) {
    if let Some(Value::Obj(uo)) = i.obj_get_raw(o, &PropKey::from_str(SP_URL)) {
        let q = sp_serialize(&sp_get_pairs(i, o));
        hid(
            i,
            &uo,
            U_QUERY,
            if q.is_empty() {
                Value::Und
            } else {
                Value::Str(Rc::from(q.as_str()))
            },
        );
    }
}

fn this_sp(i: &mut Interp, this: &Value) -> Result<ObjRef, Value> {
    let Value::Obj(o) = this else {
        return Err(i.type_err("URLSearchParams method called on non-object"));
    };
    Ok(o.clone())
}

fn sp_ctor_f(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let proto = match this {
        Value::Obj(o) => o.borrow().proto.clone(),
        _ => global_ctor_proto(i, "URLSearchParams"),
    };
    let o = match this {
        Value::Obj(o) => o.clone(),
        _ => plain(proto),
    };
    let src = arg(args, 0);
    match &src {
        Value::Und | Value::Null => sp_set_pairs(i, &o, &[]),
        Value::Str(s) => {
            let q = s.strip_prefix('?').unwrap_or(s);
            sp_set_pairs(i, &o, &sp_pairs_parse(q));
        }
        Value::Obj(_) => {
            // iterable of pairs or record
            if i.has_prop(&src, &PropKey::Sym(SYM_ITERATOR))? {
                let mut pairs = Vec::new();
                for e in i.iter_values(&src)? {
                    let kv = to_elems(i, &e)?;
                    let k = i.to_str(kv.first().unwrap_or(&Value::Und))?;
                    let v = i.to_str(kv.get(1).unwrap_or(&Value::Und))?;
                    pairs.push((k, v));
                }
                sp_set_pairs(i, &o, &pairs);
            } else {
                let mut pairs = Vec::new();
                for k in i.own_keys(&src) {
                    let v = i.get_prop(&src, &PropKey::from_str(&k))?;
                    pairs.push((k, i.to_str(&v)?));
                }
                sp_set_pairs(i, &o, &pairs);
            }
        }
        _ => sp_set_pairs(i, &o, &[]),
    }
    hid(i, &o, SP_URL, Value::Und);
    Ok(Value::Obj(o))
}

fn sp_append(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let o = this_sp(i, this)?;
    let k = i.to_str(&arg(args, 0))?;
    let v = i.to_str(&arg(args, 1))?;
    let mut pairs = sp_get_pairs(i, &o);
    pairs.push((k, v));
    sp_set_pairs(i, &o, &pairs);
    sp_sync(i, &o);
    Ok(Value::Und)
}

fn sp_delete(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let o = this_sp(i, this)?;
    let k = i.to_str(&arg(args, 0))?;
    let has_v = !matches!(arg(args, 1), Value::Und);
    let v = i.to_str(&arg(args, 1)).unwrap_or_default();
    let pairs: Vec<(String, String)> = sp_get_pairs(i, &o)
        .into_iter()
        .filter(|(k2, v2)| !(k2 == &k && (!has_v || v2 == &v)))
        .collect();
    sp_set_pairs(i, &o, &pairs);
    sp_sync(i, &o);
    Ok(Value::Und)
}

fn sp_get(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let o = this_sp(i, this)?;
    let k = i.to_str(&arg(args, 0))?;
    for (k2, v) in sp_get_pairs(i, &o) {
        if k2 == k {
            return Ok(Value::Str(Rc::from(v.as_str())));
        }
    }
    Ok(Value::Null)
}

fn sp_get_all(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let o = this_sp(i, this)?;
    let k = i.to_str(&arg(args, 0))?;
    let out: Vec<Value> = sp_get_pairs(i, &o)
        .into_iter()
        .filter(|(k2, _)| k2 == &k)
        .map(|(_, v)| Value::Str(Rc::from(v.as_str())))
        .collect();
    Ok(Value::Obj(i.new_arr(out)))
}

fn sp_has(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let o = this_sp(i, this)?;
    let k = i.to_str(&arg(args, 0))?;
    let has_v = !matches!(arg(args, 1), Value::Und);
    let v = i.to_str(&arg(args, 1)).unwrap_or_default();
    let r = sp_get_pairs(i, &o)
        .iter()
        .any(|(k2, v2)| k2 == &k && (!has_v || v2 == &v));
    Ok(Value::Bool(r))
}

fn sp_set(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let o = this_sp(i, this)?;
    let k = i.to_str(&arg(args, 0))?;
    let v = i.to_str(&arg(args, 1))?;
    let mut pairs = sp_get_pairs(i, &o);
    if let Some(pos) = pairs.iter().position(|(k2, _)| k2 == &k) {
        pairs[pos].1 = v;
        let mut seen = false;
        pairs.retain(|(k2, _)| {
            if k2 == &k {
                if seen {
                    return false;
                }
                seen = true;
            }
            true
        });
    } else {
        pairs.push((k, v));
    }
    sp_set_pairs(i, &o, &pairs);
    sp_sync(i, &o);
    Ok(Value::Und)
}

fn sp_sort(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let o = this_sp(i, this)?;
    let mut pairs = sp_get_pairs(i, &o);
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    sp_set_pairs(i, &o, &pairs);
    sp_sync(i, &o);
    Ok(Value::Und)
}

fn sp_to_string(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let o = this_sp(i, this)?;
    Ok(Value::Str(Rc::from(
        sp_serialize(&sp_get_pairs(i, &o)).as_str(),
    )))
}

fn sp_for_each(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let o = this_sp(i, this)?;
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    for (k, v) in sp_get_pairs(i, &o) {
        i.call(
            &cb,
            this_arg.clone(),
            vec![
                Value::Str(Rc::from(v.as_str())),
                Value::Str(Rc::from(k.as_str())),
                this.clone(),
            ],
        )?;
    }
    Ok(Value::Und)
}

fn sp_keys(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let o = this_sp(i, this)?;
    let ks: Vec<Value> = sp_get_pairs(i, &o)
        .into_iter()
        .map(|(k, _)| Value::Str(Rc::from(k.as_str())))
        .collect();
    Ok(make_iter(i, ks))
}

fn sp_values(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let o = this_sp(i, this)?;
    let vs: Vec<Value> = sp_get_pairs(i, &o)
        .into_iter()
        .map(|(_, v)| Value::Str(Rc::from(v.as_str())))
        .collect();
    Ok(make_iter(i, vs))
}

fn sp_entries(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let o = this_sp(i, this)?;
    let mut out = Vec::new();
    for (k, v) in sp_get_pairs(i, &o) {
        out.push(Value::Obj(i.new_arr(vec![
            Value::Str(Rc::from(k.as_str())),
            Value::Str(Rc::from(v.as_str())),
        ])));
    }
    Ok(make_iter(i, out))
}

fn sp_size(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let o = this_sp(i, this)?;
    Ok(Value::Num(sp_get_pairs(i, &o).len() as f64))
}

// ===========================================================================
// web stubs: Worker, Image, Blob, FormData, XMLHttpRequest, crypto
// ===========================================================================

fn stub_noop(_i: &mut Interp, _t: &Value, _a: &[Value]) -> NR {
    Ok(Value::Und)
}

fn install_stubs(i: &mut Interp) {
    // ---- Worker ----
    {
        let p = plain(Some(i.p.obj.clone()));
        for m in [
            "postMessage",
            "terminate",
            "addEventListener",
            "removeEventListener",
        ] {
            met(i, &p, m, stub_noop);
        }
        let ctor = mk_ctor(i, "Worker", worker_ctor, &p);
        gdef(i, "Worker", ctor);
    }
    // ---- Image ----
    {
        let p = plain(Some(i.p.obj.clone()));
        for m in ["addEventListener", "removeEventListener", "decode"] {
            met(i, &p, m, stub_noop);
        }
        let ctor = mk_ctor(i, "Image", image_ctor, &p);
        gdef(i, "Image", ctor);
    }
    // ---- Blob ----
    {
        let p = plain(Some(i.p.obj.clone()));
        met(i, &p, "text", blob_text);
        met(i, &p, "arrayBuffer", blob_array_buffer);
        met(i, &p, "slice", blob_slice);
        p.borrow_mut().props.insert(
            PropKey::Sym(SYM_TOSTRINGTAG),
            Prop::builtin(Value::Str(Rc::from("Blob"))),
        );
        let ctor = mk_ctor(i, "Blob", blob_ctor, &p);
        gdef(i, "Blob", ctor);
    }
    // ---- File = Blob subclass-ish ----
    {
        let p = plain(global_ctor_proto(i, "Blob"));
        let ctor = mk_ctor(i, "File", blob_ctor, &p);
        gdef(i, "File", ctor);
    }
    // ---- FormData ----
    {
        let p = plain(Some(i.p.obj.clone()));
        for (n, f) in [
            ("append", fd_append as NativeFn),
            ("delete", fd_delete),
            ("get", fd_get),
            ("getAll", fd_get_all),
            ("has", fd_has),
            ("set", fd_set),
            ("entries", fd_entries),
            ("keys", fd_keys),
            ("values", fd_values),
            ("forEach", fd_for_each),
        ] {
            met(i, &p, n, f);
        }
        let ent = i.obj_get_raw(&p, &PropKey::from_str("entries")).unwrap();
        p.borrow_mut()
            .props
            .insert(PropKey::Sym(SYM_ITERATOR), Prop::builtin(ent));
        let ctor = mk_ctor(i, "FormData", fd_ctor, &p);
        gdef(i, "FormData", ctor);
    }
    // ---- XMLHttpRequest ----
    {
        let p = plain(Some(i.p.obj.clone()));
        for m in [
            "open",
            "send",
            "abort",
            "setRequestHeader",
            "overrideMimeType",
            "addEventListener",
            "removeEventListener",
        ] {
            met(i, &p, m, stub_noop);
        }
        met(i, &p, "getResponseHeader", |_, _, _| Ok(Value::Null));
        met(i, &p, "getAllResponseHeaders", |_, _, _| {
            Ok(Value::Str(Rc::from("")))
        });
        let ctor = mk_ctor(i, "XMLHttpRequest", xhr_ctor, &p);
        if let Value::Obj(co) = &ctor {
            for (k, v) in [
                ("UNSENT", 0.0),
                ("OPENED", 1.0),
                ("HEADERS_RECEIVED", 2.0),
                ("LOADING", 3.0),
                ("DONE", 4.0),
            ] {
                i.set_builtin(co, k, Value::Num(v));
            }
        }
        gdef(i, "XMLHttpRequest", ctor);
    }
    // ---- Audio / Video element-ish ctor stubs ----
    for name in ["Audio", "Video", "EventSource", "WebSocket"] {
        let p = plain(Some(i.p.obj.clone()));
        for m in [
            "play",
            "pause",
            "load",
            "close",
            "send",
            "addEventListener",
            "removeEventListener",
            "dispatchEvent",
        ] {
            met(i, &p, m, stub_noop);
        }
        met(i, &p, "canPlayType", |_i, _t, _a| {
            Ok(Value::Str(Rc::from("")))
        });
        let ctor = mk_ctor(i, name, media_ctor, &p);
        gdef(i, name, ctor);
    }
    // ---- crypto ----
    {
        let c = i.new_obj();
        met(i, &c, "getRandomValues", crypto_get_random_values);
        met(i, &c, "randomUUID", crypto_random_uuid);
        i.set_builtin(&c, "subtle", Value::Obj(i.new_obj()));
        gdef(i, "crypto", Value::Obj(c));
    }
}

fn worker_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let src = i.to_str(&arg(args, 0)).unwrap_or_default();
    let o = match this {
        Value::Obj(o) => o.clone(),
        _ => plain(global_ctor_proto(i, "Worker")),
    };
    i.set_builtin(&o, "onmessage", Value::Null);
    i.set_builtin(&o, "onmessageerror", Value::Null);
    i.set_builtin(&o, "onerror", Value::Null);
    hid(i, &o, "\u{0}wurl", Value::Str(Rc::from(src.as_str())));
    Ok(Value::Obj(o))
}

fn image_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let o = match this {
        Value::Obj(o) => o.clone(),
        _ => plain(global_ctor_proto(i, "Image")),
    };
    let w = i.to_num(&arg(args, 0)).unwrap_or(0.0);
    let h = i.to_num(&arg(args, 1)).unwrap_or(0.0);
    for (k, v) in [
        ("width", Value::Num(w)),
        ("height", Value::Num(h)),
        ("naturalWidth", Value::Num(0.0)),
        ("naturalHeight", Value::Num(0.0)),
        ("src", Value::Str(Rc::from(""))),
        ("currentSrc", Value::Str(Rc::from(""))),
        ("alt", Value::Str(Rc::from(""))),
        ("crossOrigin", Value::Null),
        ("decoding", Value::Str(Rc::from("auto"))),
        ("loading", Value::Str(Rc::from("eager"))),
        ("complete", Value::Bool(false)),
        ("onload", Value::Null),
        ("onerror", Value::Null),
    ] {
        i.set_data(&o, k, v);
    }
    Ok(Value::Obj(o))
}

fn media_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let o = match this {
        Value::Obj(o) => o.clone(),
        _ => plain(this_proto(this).or_else(|| Some(i.p.obj.clone()))),
    };
    let src = i.to_str(&arg(args, 0)).unwrap_or_default();
    for (k, v) in [
        ("src", Value::Str(Rc::from(src.as_str()))),
        ("url", Value::Str(Rc::from(src.as_str()))),
        ("preload", Value::Str(Rc::from("auto"))),
        ("readyState", Value::Num(0.0)),
        ("volume", Value::Num(1.0)),
        ("currentTime", Value::Num(0.0)),
        ("duration", Value::Num(f64::NAN)),
        ("muted", Value::Bool(false)),
        ("loop", Value::Bool(false)),
        ("paused", Value::Bool(true)),
        ("ended", Value::Bool(false)),
        ("onplay", Value::Null),
        ("onpause", Value::Null),
        ("onended", Value::Null),
        ("onerror", Value::Null),
        ("oncanplaythrough", Value::Null),
        ("onmessage", Value::Null),
        ("onopen", Value::Null),
        ("onclose", Value::Null),
    ] {
        i.set_data(&o, k, v);
    }
    Ok(Value::Obj(o))
}

// -- Blob --------------------------------------------------------------------

const B_BYTES: &str = "\u{0}bb";

fn blob_bytes_of(i: &Interp, o: &ObjRef) -> Vec<u8> {
    if let Some(Value::Obj(bo)) = i.obj_get_raw(o, &PropKey::from_str(B_BYTES)) {
        if let Some(b) = buffer_bytes(&bo) {
            return b.borrow().clone();
        }
    }
    Vec::new()
}

fn blob_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let mut data: Vec<u8> = Vec::new();
    let push_part = |i: &mut Interp, data: &mut Vec<u8>, part: &Value| -> NR {
        match part {
            Value::Str(s) => data.extend_from_slice(s.as_bytes()),
            Value::Obj(_) => {
                if let Some(b) = view_bytes(part) {
                    data.extend_from_slice(&b);
                } else if let Value::Obj(po) = part {
                    data.extend_from_slice(&blob_bytes_of(i, po));
                }
            }
            other => {
                let s = i.to_str(other)?;
                data.extend_from_slice(s.as_bytes());
            }
        }
        Ok(Value::Und)
    };
    match &arg(args, 0) {
        Value::Str(s) => data.extend_from_slice(s.as_bytes()),
        Value::Obj(parts) if matches!(parts.borrow().kind, ObjKind::Arr(_)) => {
            for part in to_elems(i, &arg(args, 0))? {
                push_part(i, &mut data, &part)?;
            }
        }
        Value::Und | Value::Null => {}
        other => {
            push_part(i, &mut data, &other.clone())?;
        }
    }
    let ty = {
        let opts = arg(args, 1);
        match i.get_prop(&opts, &PropKey::from_str("type")) {
            Ok(Value::Str(s)) => s.to_lowercase(),
            _ => String::new(),
        }
    };
    let size = data.len();
    let o = match this {
        Value::Obj(o) => o.clone(),
        _ => plain(global_ctor_proto(i, "Blob")),
    };
    let buf = mk_buffer(i, data);
    hid(i, &o, B_BYTES, Value::Obj(buf));
    i.set_builtin(&o, "size", Value::Num(size as f64));
    i.set_builtin(&o, "type", Value::Str(Rc::from(ty.as_str())));
    Ok(Value::Obj(o))
}

fn blob_text(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let s = if let Value::Obj(o) = this {
        String::from_utf8_lossy(&blob_bytes_of(i, o)).to_string()
    } else {
        String::new()
    };
    promise_resolve_static(i, &Value::Und, &[Value::Str(Rc::from(s.as_str()))])
}

fn blob_array_buffer(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let data = if let Value::Obj(o) = this {
        blob_bytes_of(i, o)
    } else {
        Vec::new()
    };
    let buf = Value::Obj(mk_buffer(i, data));
    promise_resolve_static(i, &Value::Und, &[buf])
}

fn blob_slice(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let data = if let Value::Obj(o) = this {
        blob_bytes_of(i, o)
    } else {
        Vec::new()
    };
    let len = data.len();
    let start = clamp_index(i.to_num(&arg(args, 0)).unwrap_or(0.0), len);
    let end = match arg(args, 1) {
        Value::Und => len,
        v => clamp_index(i.to_num(&v).unwrap_or(0.0), len),
    };
    let out = if start < end {
        data[start..end].to_vec()
    } else {
        Vec::new()
    };
    let ty = i.to_str(&arg(args, 2)).unwrap_or_default().to_lowercase();
    let proto = this_proto(this);
    let o = plain(proto);
    let sz = out.len();
    hid(i, &o, B_BYTES, Value::Obj(mk_buffer(i, out)));
    i.set_builtin(&o, "size", Value::Num(sz as f64));
    i.set_builtin(&o, "type", Value::Str(Rc::from(ty.as_str())));
    Ok(Value::Obj(o))
}

// -- FormData ----------------------------------------------------------------

const F_ENTS: &str = "\u{0}fe";

fn fd_entries_of(i: &Interp, o: &ObjRef) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    if let Some(Value::Obj(arr)) = i.obj_get_raw(o, &PropKey::from_str(F_ENTS)) {
        if let ObjKind::Arr(items) = &arr.borrow().kind {
            for it in items {
                if let Value::Obj(p) = it {
                    if let ObjKind::Arr(kv) = &p.borrow().kind {
                        if kv.len() >= 2 {
                            let k = match &kv[0] {
                                Value::Str(s) => s.to_string(),
                                _ => String::new(),
                            };
                            out.push((k, kv[1].clone()));
                        }
                    }
                }
            }
        }
    }
    out
}

fn fd_set_entries(i: &Interp, o: &ObjRef, ents: &[(String, Value)]) {
    let items: Vec<Value> = ents
        .iter()
        .map(|(k, v)| Value::Obj(i.new_arr(vec![Value::Str(Rc::from(k.as_str())), v.clone()])))
        .collect();
    hid(i, o, F_ENTS, Value::Obj(i.new_arr(items)));
}

fn fd_ctor(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let o = match this {
        Value::Obj(o) => o.clone(),
        _ => plain(global_ctor_proto(i, "FormData")),
    };
    fd_set_entries(i, &o, &[]);
    Ok(Value::Obj(o))
}

fn this_fd(i: &mut Interp, this: &Value) -> Result<ObjRef, Value> {
    let Value::Obj(o) = this else {
        return Err(i.type_err("FormData method called on non-object"));
    };
    Ok(o.clone())
}

fn fd_append(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let o = this_fd(i, this)?;
    let k = i.to_str(&arg(args, 0))?;
    let mut ents = fd_entries_of(i, &o);
    ents.push((k, arg(args, 1)));
    fd_set_entries(i, &o, &ents);
    Ok(Value::Und)
}

fn fd_delete(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let o = this_fd(i, this)?;
    let k = i.to_str(&arg(args, 0))?;
    let ents: Vec<(String, Value)> = fd_entries_of(i, &o)
        .into_iter()
        .filter(|(k2, _)| k2 != &k)
        .collect();
    fd_set_entries(i, &o, &ents);
    Ok(Value::Und)
}

fn fd_get(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let o = this_fd(i, this)?;
    let k = i.to_str(&arg(args, 0))?;
    for (k2, v) in fd_entries_of(i, &o) {
        if k2 == k {
            return Ok(v);
        }
    }
    Ok(Value::Null)
}

fn fd_get_all(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let o = this_fd(i, this)?;
    let k = i.to_str(&arg(args, 0))?;
    let out: Vec<Value> = fd_entries_of(i, &o)
        .into_iter()
        .filter(|(k2, _)| k2 == &k)
        .map(|(_, v)| v)
        .collect();
    Ok(Value::Obj(i.new_arr(out)))
}

fn fd_has(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let o = this_fd(i, this)?;
    let k = i.to_str(&arg(args, 0))?;
    Ok(Value::Bool(
        fd_entries_of(i, &o).iter().any(|(k2, _)| k2 == &k),
    ))
}

fn fd_set(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let o = this_fd(i, this)?;
    let k = i.to_str(&arg(args, 0))?;
    let v = arg(args, 1);
    let mut ents = fd_entries_of(i, &o);
    if let Some(pos) = ents.iter().position(|(k2, _)| k2 == &k) {
        ents[pos].1 = v;
        let mut seen = false;
        ents.retain(|(k2, _)| {
            if k2 == &k {
                if seen {
                    return false;
                }
                seen = true;
            }
            true
        });
    } else {
        ents.push((k, v));
    }
    fd_set_entries(i, &o, &ents);
    Ok(Value::Und)
}

fn fd_entries(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let o = this_fd(i, this)?;
    let mut out = Vec::new();
    for (k, v) in fd_entries_of(i, &o) {
        out.push(Value::Obj(
            i.new_arr(vec![Value::Str(Rc::from(k.as_str())), v]),
        ));
    }
    Ok(make_iter(i, out))
}

fn fd_keys(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let o = this_fd(i, this)?;
    let ks: Vec<Value> = fd_entries_of(i, &o)
        .into_iter()
        .map(|(k, _)| Value::Str(Rc::from(k.as_str())))
        .collect();
    Ok(make_iter(i, ks))
}

fn fd_values(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let o = this_fd(i, this)?;
    let vs: Vec<Value> = fd_entries_of(i, &o).into_iter().map(|(_, v)| v).collect();
    Ok(make_iter(i, vs))
}

fn fd_for_each(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let o = this_fd(i, this)?;
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    for (k, v) in fd_entries_of(i, &o) {
        i.call(
            &cb,
            this_arg.clone(),
            vec![v, Value::Str(Rc::from(k.as_str())), this.clone()],
        )?;
    }
    Ok(Value::Und)
}

// -- XMLHttpRequest -----------------------------------------------------------

fn xhr_ctor(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let o = match this {
        Value::Obj(o) => o.clone(),
        _ => plain(global_ctor_proto(i, "XMLHttpRequest")),
    };
    for (k, v) in [
        ("readyState", Value::Num(0.0)),
        ("status", Value::Num(0.0)),
        ("statusText", Value::Str(Rc::from(""))),
        ("response", Value::Str(Rc::from(""))),
        ("responseText", Value::Str(Rc::from(""))),
        ("responseXML", Value::Null),
        ("responseType", Value::Str(Rc::from(""))),
        ("responseURL", Value::Str(Rc::from(""))),
        ("timeout", Value::Num(0.0)),
        ("withCredentials", Value::Bool(false)),
        ("onreadystatechange", Value::Null),
        ("onload", Value::Null),
        ("onloadstart", Value::Null),
        ("onloadend", Value::Null),
        ("onerror", Value::Null),
        ("onabort", Value::Null),
        ("ontimeout", Value::Null),
        ("onprogress", Value::Null),
        ("upload", Value::Obj(i.new_obj())),
        ("UNSENT", Value::Num(0.0)),
        ("OPENED", Value::Num(1.0)),
        ("HEADERS_RECEIVED", Value::Num(2.0)),
        ("LOADING", Value::Num(3.0)),
        ("DONE", Value::Num(4.0)),
    ] {
        i.set_data(&o, k, v);
    }
    Ok(Value::Obj(o))
}

// -- crypto --------------------------------------------------------------------

fn crypto_fill(i: &mut Interp, n: usize) -> Vec<u8> {
    // /dev/urandom first; xorshift fallback (std-only, fine for a stub)
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let mut buf = vec![0u8; n];
        if f.read_exact(&mut buf).is_ok() {
            return buf;
        }
    }
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let mut x = i.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        i.rng = x;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(n);
    out
}

fn crypto_get_random_values(i: &mut Interp, _t: &Value, args: &[Value]) -> NR {
    let v = arg(args, 0);
    match ta_parts(&v) {
        Some(tp) => {
            let n = tp.eff().unwrap_or(0) * tp.kind.size();
            let rnd = crypto_fill(i, n);
            if let Some(b) = buffer_bytes(&tp.buf) {
                b.borrow_mut()[tp.off..tp.off + n].copy_from_slice(&rnd);
            }
            Ok(v)
        }
        None => match dv_parts(&v) {
            Some((b, off, len)) => {
                let rnd = crypto_fill(i, len);
                b.borrow_mut()[off..off + len].copy_from_slice(&rnd);
                Ok(v)
            }
            None => Err(i.type_err("getRandomValues requires a TypedArray")),
        },
    }
}

fn crypto_random_uuid(i: &mut Interp, _t: &Value, _a: &[Value]) -> NR {
    let mut b = crypto_fill(i, 16);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let s = format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    );
    Ok(Value::Str(Rc::from(s.as_str())))
}
