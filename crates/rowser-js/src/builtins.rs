//! Minimal standard global environment.

use crate::interp::*;
use crate::regexp::{Regex, RxMatch};
use std::cell::RefCell;
use std::rc::Rc;

type NResult = Result<Value, Value>;

fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).cloned().unwrap_or(Value::Und)
}

fn native(i: &Interp, name: &str, f: NativeFn) -> Value {
    i.native(name, f)
}

fn method(i: &Interp, proto: &ObjRef, name: &str, f: NativeFn) {
    let v = native(i, name, f);
    i.set_builtin(proto, name, v);
}

// -- helpers used by natives --------------------------------------------------

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
            if let ObjKind::Arr(items) = &o.borrow().kind {
                return Ok(items.clone());
            }
            if let ObjKind::Iter { items, .. } = &o.borrow().kind {
                return Ok(items.clone());
            }
            let len = i.get_prop(v, &PropKey::from_str("length"))?;
            let n = i.to_num(&len)? as usize;
            let mut out = Vec::with_capacity(n.min(1_000_000));
            for k in 0..n {
                out.push(i.get_prop(v, &PropKey::from_str(&k.to_string()))?);
            }
            Ok(out)
        }
        _ => Ok(Vec::new()),
    }
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
    i.declare(&i.global.clone(), name, v.clone(), true);
    // declare() already mirrors to global object
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
        method(i, &p, "toLocaleString", obj_to_string);
        method(i, &p, "valueOf", obj_value_of);
        // __proto__ accessor
        let get = native(i, "get __proto__", obj_get_proto);
        let set = native(i, "set __proto__", obj_set_proto);
        p.borrow_mut().props.insert(
            PropKey::from_str("__proto__"),
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
    }

    // -- Array.prototype --
    {
        let p = i.p.array.clone();
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
        let values_fn = i.obj_get_raw(&p, &PropKey::from_str("values")).unwrap();
        p.borrow_mut()
            .props
            .insert(PropKey::Sym(SYM_ITERATOR), Prop::builtin(values_fn));
    }

    // -- String.prototype --
    {
        let p = i.p.string.clone();
        method(i, &p, "charAt", str_char_at);
        method(i, &p, "charCodeAt", str_char_code_at);
        method(i, &p, "codePointAt", str_char_code_at);
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
        method(i, &p, "trimLeft", str_trim_start);
        method(i, &p, "trimRight", str_trim_end);
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
        method(i, &p, "anchor", str_to_string);
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
        let get = native(i, "get description", sym_get_description);
        p.borrow_mut().props.insert(
            PropKey::from_str("description"),
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
        ] {
            proto.borrow_mut().proto = Some(p.clone());
            i.set_builtin(&proto, "name", Value::Str(Rc::from(name)));
            i.set_builtin(&proto, "message", Value::Str(Rc::from("")));
        }
    }

    // -- iterator proto: [Symbol.iterator]() { return this } --
    {
        let p = i.p.iterator.clone();
        let f = native(i, "[Symbol.iterator]", iter_self);
        p.borrow_mut()
            .props
            .insert(PropKey::Sym(SYM_ITERATOR), Prop::builtin(f));
        method(i, &p, "next", iter_next);
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
            ("hasIndices", re_get_has_indices),
        ] {
            let get = native(i, &format!("get {name}"), f);
            p.borrow_mut()
                .props
                .insert(PropKey::from_str(name), Prop::accessor(Some(get), None));
        }
        for (sym, name, f) in [
            (SYM_MATCH, "[Symbol.match]", re_sym_match as NativeFn),
            (SYM_MATCHALL, "[Symbol.matchAll]", re_sym_match_all),
            (SYM_REPLACE, "[Symbol.replace]", re_sym_replace),
            (SYM_SEARCH, "[Symbol.search]", re_sym_search),
            (SYM_SPLIT, "[Symbol.split]", re_sym_split),
        ] {
            let v = native(i, name, f);
            p.borrow_mut().props.insert(PropKey::Sym(sym), Prop::builtin(v));
        }
        i.set_builtin(&p, "lastIndex", Value::Num(0.0));
    }
}

fn iter_self(_i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    Ok(this.clone())
}

fn iter_next(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let mut v = Value::Und;
    let mut done = true;
    if let Value::Obj(o) = this {
        if let ObjKind::Iter { items, idx } = &mut o.borrow_mut().kind {
            if *idx < items.len() {
                v = items[*idx].clone();
                *idx += 1;
                done = false;
            }
        }
    }
    let r = i.new_obj();
    i.set_data(&r, "value", v);
    i.set_data(&r, "done", Value::Bool(done));
    Ok(Value::Obj(r))
}

pub(crate) fn make_iter(i: &mut Interp, items: Vec<Value>) -> Value {
    let o = Rc::new(RefCell::new(Obj::new(
        Some(i.p.iterator.clone()),
        ObjKind::Iter { items, idx: 0 },
    )));
    Value::Obj(o)
}

// ---------------------------------------------------------------------------
// globals
// ---------------------------------------------------------------------------

fn install_globals(i: &mut Interp) {
    define_global(i, "undefined", Value::Und);
    define_global(i, "NaN", Value::Num(f64::NAN));
    define_global(i, "Infinity", Value::Num(f64::INFINITY));
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
            ("getOwnPropertyNames", obj_keys),
            ("getPrototypeOf", obj_get_prototype_of),
            ("setPrototypeOf", obj_set_prototype_of),
            ("create", obj_create),
            ("freeze", obj_freeze),
            ("seal", obj_freeze),
            ("isFrozen", obj_is_frozen),
            ("isSealed", obj_is_frozen),
            ("preventExtensions", obj_identity),
            ("isExtensible", obj_is_extensible),
            ("fromEntries", obj_from_entries),
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
        i.set_builtin(o, "isArray", native(i, "isArray", array_is_array));
        i.set_builtin(o, "from", native(i, "from", array_from));
        i.set_builtin(o, "of", native(i, "of", array_of));
        define_global(i, "Array", ctor);
    }

    // -- String --
    {
        let ctor = make_ctor(i, "String", string_ctor, i.p.string.clone());
        let Value::Obj(o) = &ctor else { unreachable!() };
        i.set_builtin(o, "fromCharCode", native(i, "fromCharCode", string_from_char_code));
        i.set_builtin(o, "fromCodePoint", native(i, "fromCodePoint", string_from_char_code));
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
            i.set_builtin(o, n, Value::Num(v));
        }
        define_global(i, "Number", ctor);
    }

    // -- Boolean --
    {
        let ctor = make_ctor(i, "Boolean", boolean_ctor, i.p.boolean.clone());
        define_global(i, "Boolean", ctor);
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
        i.set_builtin(o, "for", native(i, "for", symbol_for));
        i.set_builtin(o, "keyFor", native(i, "keyFor", symbol_key_for));
        i.set_builtin(o, "hasInstance", Value::Sym(SYM_HASINSTANCE, Rc::from("Symbol.hasInstance")));
        i.set_builtin(o, "match", Value::Sym(SYM_MATCH, Rc::from("Symbol.match")));
        i.set_builtin(o, "matchAll", Value::Sym(SYM_MATCHALL, Rc::from("Symbol.matchAll")));
        i.set_builtin(o, "replace", Value::Sym(SYM_REPLACE, Rc::from("Symbol.replace")));
        i.set_builtin(o, "search", Value::Sym(SYM_SEARCH, Rc::from("Symbol.search")));
        i.set_builtin(o, "split", Value::Sym(SYM_SPLIT, Rc::from("Symbol.split")));
        define_global(i, "Symbol", ctor);
    }

    // -- RegExp --
    {
        let ctor = make_ctor(i, "RegExp", regexp_ctor, i.p.regexp.clone());
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
            i.set_builtin(&m, n, Value::Num(v));
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
        let se = make_ctor(i, "SyntaxError", syntax_error_ctor, i.p.syntax_error.clone());
        define_global(i, "SyntaxError", se);
        let fe = make_ctor(i, "ReferenceError", ref_error_ctor, i.p.ref_error.clone());
        define_global(i, "ReferenceError", fe);
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
        method(i, &dp, "toJSON", date_to_iso);
        method(i, &dp, "toUTCString", date_to_utc);
        method(i, &dp, "toGMTString", date_to_utc);
        method(i, &dp, "toDateString", date_to_datestr);
        method(i, &dp, "toTimeString", date_to_timestr);
        method(i, &dp, "toLocaleString", date_to_utc);
        method(i, &dp, "toLocaleDateString", date_to_datestr);
        method(i, &dp, "toLocaleTimeString", date_to_timestr);
        method(i, &dp, "getFullYear", |i, t, _| date_getter(i, t, |d| d.0 as f64));
        method(i, &dp, "getMonth", |i, t, _| date_getter(i, t, |d| (d.1 - 1) as f64));
        method(i, &dp, "getDate", |i, t, _| date_getter(i, t, |d| d.2 as f64));
        method(i, &dp, "getDay", |i, t, _| date_getter(i, t, |d| d.3 as f64));
        method(i, &dp, "getHours", |i, t, _| date_getter(i, t, |d| d.4 as f64));
        method(i, &dp, "getMinutes", |i, t, _| date_getter(i, t, |d| d.5 as f64));
        method(i, &dp, "getSeconds", |i, t, _| date_getter(i, t, |d| d.6 as f64));
        method(i, &dp, "getMilliseconds", |i, t, _| date_getter(i, t, |d| d.7 as f64));
        method(i, &dp, "getUTCFullYear", |i, t, _| date_getter(i, t, |d| d.0 as f64));
        method(i, &dp, "getUTCMonth", |i, t, _| date_getter(i, t, |d| (d.1 - 1) as f64));
        method(i, &dp, "getUTCDate", |i, t, _| date_getter(i, t, |d| d.2 as f64));
        method(i, &dp, "getUTCDay", |i, t, _| date_getter(i, t, |d| d.3 as f64));
        method(i, &dp, "getUTCHours", |i, t, _| date_getter(i, t, |d| d.4 as f64));
        method(i, &dp, "getUTCMinutes", |i, t, _| date_getter(i, t, |d| d.5 as f64));
        method(i, &dp, "getUTCSeconds", |i, t, _| date_getter(i, t, |d| d.6 as f64));
        method(i, &dp, "getUTCMilliseconds", |i, t, _| date_getter(i, t, |d| d.7 as f64));
        method(i, &dp, "getTimezoneOffset", |_i, _t, _| Ok(Value::Num(0.0)));
        let ctor = make_ctor(i, "Date", date_ctor, dp.clone());
        let Value::Obj(o) = &ctor else { unreachable!() };
        i.set_builtin(o, "now", native(i, "now", date_now));
        i.set_builtin(o, "UTC", native(i, "UTC", date_utc));
        i.set_builtin(o, "parse", native(i, "parse", date_parse));
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
        let ent = i.obj_get_raw(&mp, &PropKey::from_str("entries")).unwrap();
        mp.borrow_mut().props.insert(PropKey::Sym(SYM_ITERATOR), Prop::builtin(ent));
        let size_g = native(i, "get size", map_size);
        mp.borrow_mut()
            .props
            .insert(PropKey::from_str("size"), Prop::accessor(Some(size_g), None));
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
        let sv = i.obj_get_raw(&sp, &PropKey::from_str("values")).unwrap();
        sp.borrow_mut().props.insert(PropKey::Sym(SYM_ITERATOR), Prop::builtin(sv));
        let ssize = native(i, "get size", set_size);
        sp.borrow_mut()
            .props
            .insert(PropKey::from_str("size"), Prop::accessor(Some(ssize), None));
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
    ] {
        define_global(i, n, native(i, n, f));
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
        method(i, &r, "ownKeys", obj_keys);
        method(i, &r, "getPrototypeOf", obj_get_prototype_of);
        method(i, &r, "setPrototypeOf", obj_set_prototype_of);
        method(i, &r, "apply", reflect_apply);
        method(i, &r, "construct", reflect_construct);
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
    i.set_builtin(&o, "name", Value::Str(Rc::from(name)));
    i.set_builtin(&o, "prototype", Value::Obj(proto.clone()));
    i.set_builtin(&proto, "constructor", Value::Obj(o.clone()));
    Value::Obj(o)
}

// ===========================================================================
// natives: object
// ===========================================================================

fn obj_has_own(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let k = i.to_propkey(&arg(args, 0))?;
    let own = if let Value::Obj(o) = this {
        if let ObjKind::Arr(items) = &o.borrow().kind {
            if let PropKey::S(s) = &k {
                if s.as_ref() == "length" {
                    return Ok(Value::Bool(true));
                }
                if let Some(idx) = arr_index(s) {
                    return Ok(Value::Bool(idx < items.len()));
                }
            }
        }
        o.borrow().props.contains_key(&k)
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
    if let Value::Obj(o) = this {
        if let Some(p) = o.borrow().props.get(&k) {
            return Ok(Value::Bool(p.enumerable));
        }
        if let ObjKind::Arr(items) = &o.borrow().kind {
            if let PropKey::S(s) = &k {
                if let Some(idx) = arr_index(s) {
                    return Ok(Value::Bool(idx < items.len()));
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
        Value::Obj(o) => match &o.borrow().kind {
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
        },
    };
    let _ = i;
    Ok(Value::Str(Rc::from(format!("[object {tag}]").as_str())))
}

fn obj_value_of(_i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    Ok(this.clone())
}

fn obj_get_proto(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let _ = i;
    match this {
        Value::Obj(o) => Ok(match o.borrow().proto.clone() {
            Some(p) => Value::Obj(p),
            None => Value::Null,
        }),
        _ => Ok(Value::Und),
    }
}

fn obj_set_proto(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let _ = i;
    if let (Value::Obj(o), v) = (this, &arg(args, 0)) {
        match v {
            Value::Obj(p) => o.borrow_mut().proto = Some(p.clone()),
            Value::Null => o.borrow_mut().proto = None,
            _ => {}
        }
    }
    Ok(Value::Und)
}

fn obj_keys(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let keys = i.own_keys(&arg(args, 0));
    Ok(Value::Obj(i.new_arr(
        keys.into_iter().map(|k| Value::Str(Rc::from(k.as_str()))).collect(),
    )))
}

fn obj_values(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let v = arg(args, 0);
    let mut out = Vec::new();
    for k in i.own_keys(&v) {
        out.push(i.get_prop(&v, &PropKey::from_str(&k))?);
    }
    Ok(Value::Obj(i.new_arr(out)))
}

fn obj_entries(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let v = arg(args, 0);
    let mut out = Vec::new();
    for k in i.own_keys(&v) {
        let val = i.get_prop(&v, &PropKey::from_str(&k))?;
        out.push(Value::Obj(i.new_arr(vec![Value::Str(Rc::from(k.as_str())), val])));
    }
    Ok(Value::Obj(i.new_arr(out)))
}

fn obj_assign(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let target = arg(args, 0);
    if matches!(target, Value::Und | Value::Null) {
        return Err(i.type_err("Cannot convert undefined or null to object"));
    }
    for src in args.iter().skip(1) {
        if matches!(src, Value::Und | Value::Null) {
            continue;
        }
        // own enumerable string keys
        for k in i.own_keys(src) {
            let v = i.get_prop(src, &PropKey::from_str(&k))?;
            i.set_prop(&target, PropKey::from_str(&k), v)?;
        }
        // enumerable symbol props
        if let Value::Obj(so) = src {
            let syms: Vec<PropKey> = so
                .borrow()
                .props
                .iter()
                .filter(|(_, p)| p.enumerable)
                .filter(|(k, _)| matches!(k, PropKey::Sym(_)))
                .map(|(k, _)| k.clone())
                .collect();
            for k in syms {
                let v = i.get_prop(src, &k)?;
                i.set_prop(&target, k, v)?;
            }
        }
    }
    Ok(target)
}

fn read_desc_prop(i: &mut Interp, desc: &Value, name: &str) -> Result<(bool, Value), Value> {
    let v = i.get_prop(desc, &PropKey::from_str(name))?;
    Ok((!matches!(v, Value::Und), v))
}

fn obj_define_property(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let target = arg(args, 0);
    let Value::Obj(o) = &target else {
        return Err(i.type_err("Object.defineProperty called on non-object"));
    };
    let key = i.to_propkey(&arg(args, 1))?;
    let desc = arg(args, 2);
    if !matches!(desc, Value::Obj(_)) {
        return Err(i.type_err("Property description must be an object"));
    }
    define_prop_from_desc(i, o, key, &desc)?;
    Ok(target)
}

fn define_prop_from_desc(i: &mut Interp, o: &ObjRef, key: PropKey, desc: &Value) -> Result<(), Value> {
    let mut prop = {
        let b = o.borrow();
        b.props.get(&key).cloned().unwrap_or(Prop {
            val: Value::Und,
            writable: true,
            enumerable: true,
            configurable: true,
            get: None,
            set: None,
        })
    };
    // accessor?
    let (has_get, gv) = read_desc_prop(i, desc, "get")?;
    let (has_set, sv) = read_desc_prop(i, desc, "set")?;
    let (has_val, vv) = read_desc_prop(i, desc, "value")?;
    if has_get || has_set {
        prop.get = if has_get { Some(gv) } else { prop.get };
        prop.set = if has_set { Some(sv) } else { prop.set };
        prop.writable = false;
        prop.enumerable = false;
    }
    if has_val {
        prop.val = vv;
        prop.get = None;
        prop.set = None;
    }
    let (has_w, wv) = read_desc_prop(i, desc, "writable")?;
    if has_w {
        prop.writable = i.to_bool(&wv);
    }
    let (has_e, ev) = read_desc_prop(i, desc, "enumerable")?;
    if has_e {
        prop.enumerable = i.to_bool(&ev);
    }
    let (has_c, cv) = read_desc_prop(i, desc, "configurable")?;
    if has_c {
        prop.configurable = i.to_bool(&cv);
    }
    o.borrow_mut().props.insert(key, prop);
    Ok(())
}

fn obj_define_properties(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let target = arg(args, 0);
    let Value::Obj(o) = &target else {
        return Err(i.type_err("Object.defineProperties called on non-object"));
    };
    let descs = arg(args, 1);
    for k in i.own_keys(&descs) {
        let d = i.get_prop(&descs, &PropKey::from_str(&k))?;
        define_prop_from_desc(i, o, PropKey::from_str(&k), &d)?;
    }
    Ok(target)
}

fn obj_get_own_prop_desc(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let v = arg(args, 0);
    let key = i.to_propkey(&arg(args, 1))?;
    let Value::Obj(o) = &v else {
        return Ok(Value::Und);
    };
    // array index/length
    if let ObjKind::Arr(items) = &o.borrow().kind {
        if let PropKey::S(s) = &key {
            if let Some(idx) = arr_index(s) {
                if idx < items.len() {
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
    let v = arg(args, 0);
    let out = i.new_obj();
    for k in i.own_keys(&v) {
        let d = obj_get_own_prop_desc(
            i,
            &Value::Und,
            &[v.clone(), Value::Str(Rc::from(k.as_str()))],
        )?;
        i.set_data(&out, &k, d);
    }
    Ok(Value::Obj(out))
}

fn obj_get_prototype_of(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let _ = i;
    match arg(args, 0) {
        Value::Obj(o) => Ok(match o.borrow().proto.clone() {
            Some(p) => Value::Obj(p),
            None => Value::Null,
        }),
        _ => Ok(Value::Null),
    }
}

fn obj_set_prototype_of(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let _ = i;
    let t = arg(args, 0);
    if let (Value::Obj(o), v) = (&t, &arg(args, 1)) {
        match v {
            Value::Obj(p) => o.borrow_mut().proto = Some(p.clone()),
            Value::Null => o.borrow_mut().proto = None,
            _ => {}
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
    if let Value::Obj(_) = &descs {
        for k in i.own_keys(&descs) {
            let d = i.get_prop(&descs, &PropKey::from_str(&k))?;
            define_prop_from_desc(i, &o, PropKey::from_str(&k), &d)?;
        }
    }
    Ok(Value::Obj(o))
}

fn obj_freeze(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let _ = i;
    let v = arg(args, 0);
    if let Value::Obj(o) = &v {
        let mut b = o.borrow_mut();
        for p in b.props.values_mut() {
            p.writable = false;
            p.configurable = false;
        }
        b.props
            .insert(PropKey::from_str("$frozen"), Prop::builtin(Value::Bool(true)));
    }
    Ok(v)
}

fn obj_is_frozen(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let _ = i;
    if let Value::Obj(o) = &arg(args, 0) {
        return Ok(Value::Bool(
            o.borrow().props.contains_key(&PropKey::from_str("$frozen")),
        ));
    }
    Ok(Value::Bool(true))
}

fn obj_identity(_i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    Ok(arg(args, 0))
}

fn obj_is_extensible(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let _ = i;
    Ok(Value::Bool(matches!(arg(args, 0), Value::Obj(_))))
}

fn obj_from_entries(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let o = i.new_obj();
    let items = i.iter_values(&arg(args, 0))?;
    for e in items {
        let k = i.get_prop(&e, &PropKey::from_str("0"))?;
        let v = i.get_prop(&e, &PropKey::from_str("1"))?;
        let key = i.to_propkey(&k)?;
        i.set_prop(&Value::Obj(o.clone()), key, v)?;
    }
    Ok(Value::Obj(o))
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
    let mut out = Vec::new();
    if let Value::Obj(o) = &arg(args, 0) {
        for (k, _) in o.borrow().props.iter() {
            if let PropKey::Sym(id) = k {
                out.push(Value::Sym(*id, Rc::from("")));
            }
        }
    }
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
    let Value::Obj(_) = this else {
        return Err(i.type_err("Bind must be called on a function"));
    };
    let bound = Rc::new(RefCell::new(Obj::new(
        Some(i.p.func.clone()),
        ObjKind::Bound {
            target: this.clone(),
            thisv: arg(args, 0),
            extra: args.iter().skip(1).cloned().collect(),
        },
    )));
    i.set_builtin(&bound, "name", Value::Str(Rc::from("bound")));
    Ok(Value::Obj(bound))
}

fn fn_to_string(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let _ = i;
    let name = if let Value::Obj(o) = this {
        match &o.borrow().kind {
            ObjKind::Func(Func::User { def, .. }) => {
                def.name.clone().unwrap_or_default()
            }
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
    let mut len = 0;
    with_arr(this, |items| {
        items.extend(args.iter().cloned());
        len = items.len();
    });
    Ok(Value::Num(len as f64))
}

fn arr_pop(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let _ = i;
    let mut out = Value::Und;
    with_arr(this, |items| {
        out = items.pop().unwrap_or(Value::Und);
    });
    Ok(out)
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
    let items = arr_clone(this);
    let len = items.len();
    let start = clamp_index(i.to_num(&arg(args, 0)).unwrap_or(0.0), len);
    let end = match arg(args, 1) {
        Value::Und => len,
        v => clamp_index(i.to_num(&v).unwrap_or(0.0), len),
    };
    let out = if start < end { items[start..end].to_vec() } else { Vec::new() };
    Ok(Value::Obj(i.new_arr(out)))
}

fn arr_splice(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let mut removed = Vec::new();
    if let Value::Obj(o) = this {
        if let ObjKind::Arr(items) = &mut o.borrow_mut().kind {
            let len = items.len();
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
            let tail: Vec<Value> = items.split_off(start);
            removed = tail[..del.min(tail.len())].to_vec();
            let rest = tail[del.min(tail.len())..].to_vec();
            for (k, v) in args.iter().skip(2).enumerate() {
                items.insert(start + k, v.clone());
            }
            items.extend(rest);
        }
    }
    Ok(Value::Obj(i.new_arr(removed)))
}

fn arr_concat(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let mut out = arr_clone(this);
    for a in args {
        match a {
            Value::Obj(o) if matches!(o.borrow().kind, ObjKind::Arr(_)) => {
                out.extend(arr_clone(a));
            }
            other => out.push(other.clone()),
        }
    }
    Ok(Value::Obj(i.new_arr(out)))
}

fn arr_join(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let sep = match arg(args, 0) {
        Value::Und => ",".to_string(),
        v => i.to_str(&v)?,
    };
    let mut parts = Vec::new();
    for it in arr_clone(this) {
        match it {
            Value::Und | Value::Null => parts.push(String::new()),
            v => parts.push(i.to_str(&v)?),
        }
    }
    Ok(Value::Str(Rc::from(parts.join(&sep).as_str())))
}

fn arr_index_of(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let items = arr_clone(this);
    let needle = arg(args, 0);
    let start = clamp_index(i.to_num(&arg(args, 1)).unwrap_or(0.0), items.len());
    for (k, v) in items.iter().enumerate().skip(start) {
        if strict_val_eq(v, &needle) {
            return Ok(Value::Num(k as f64));
        }
    }
    Ok(Value::Num(-1.0))
}

fn arr_last_index_of(_i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let items = arr_clone(this);
    let needle = arg(args, 0);
    for k in (0..items.len()).rev() {
        if strict_val_eq(&items[k], &needle) {
            return Ok(Value::Num(k as f64));
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
    let _ = i;
    let items = arr_clone(this);
    let needle = arg(args, 0);
    for v in &items {
        if same_value_zero(v, &needle) {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

fn arr_find(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    let items = arr_clone(this);
    for (k, v) in items.iter().enumerate() {
        let r = i.call(&cb, this_arg.clone(), vec![v.clone(), Value::Num(k as f64), this.clone()])?;
        if i.to_bool(&r) {
            return Ok(v.clone());
        }
    }
    Ok(Value::Und)
}

fn arr_find_index(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    let items = arr_clone(this);
    for (k, v) in items.iter().enumerate() {
        let r = i.call(&cb, this_arg.clone(), vec![v.clone(), Value::Num(k as f64), this.clone()])?;
        if i.to_bool(&r) {
            return Ok(Value::Num(k as f64));
        }
    }
    Ok(Value::Num(-1.0))
}

fn arr_find_last(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    let items = arr_clone(this);
    for k in (0..items.len()).rev() {
        let r = i.call(&cb, this_arg.clone(), vec![items[k].clone(), Value::Num(k as f64), this.clone()])?;
        if i.to_bool(&r) {
            return Ok(items[k].clone());
        }
    }
    Ok(Value::Und)
}

fn arr_find_last_index(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    let items = arr_clone(this);
    for k in (0..items.len()).rev() {
        let r = i.call(&cb, this_arg.clone(), vec![items[k].clone(), Value::Num(k as f64), this.clone()])?;
        if i.to_bool(&r) {
            return Ok(Value::Num(k as f64));
        }
    }
    Ok(Value::Num(-1.0))
}

fn arr_filter(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    let items = arr_clone(this);
    let mut out = Vec::new();
    for (k, v) in items.iter().enumerate() {
        let r = i.call(&cb, this_arg.clone(), vec![v.clone(), Value::Num(k as f64), this.clone()])?;
        if i.to_bool(&r) {
            out.push(v.clone());
        }
    }
    Ok(Value::Obj(i.new_arr(out)))
}

fn arr_map(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    let items = arr_clone(this);
    let mut out = Vec::with_capacity(items.len());
    for (k, v) in items.iter().enumerate() {
        out.push(i.call(&cb, this_arg.clone(), vec![v.clone(), Value::Num(k as f64), this.clone()])?);
    }
    Ok(Value::Obj(i.new_arr(out)))
}

fn arr_for_each(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    let items = arr_clone(this);
    for (k, v) in items.iter().enumerate() {
        i.call(&cb, this_arg.clone(), vec![v.clone(), Value::Num(k as f64), this.clone()])?;
    }
    Ok(Value::Und)
}

fn arr_reduce(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let items = arr_clone(this);
    let (mut acc, start) = match args.get(1) {
        Some(v) => (v.clone(), 0),
        None => {
            if items.is_empty() {
                return Err(i.type_err("Reduce of empty array with no initial value"));
            }
            (items[0].clone(), 1)
        }
    };
    for (k, v) in items.iter().enumerate().skip(start) {
        acc = i.call(
            &cb,
            Value::Und,
            vec![acc, v.clone(), Value::Num(k as f64), this.clone()],
        )?;
    }
    Ok(acc)
}

fn arr_reduce_right(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let items = arr_clone(this);
    let (mut acc, start) = match args.get(1) {
        Some(v) => (v.clone(), items.len()),
        None => {
            if items.is_empty() {
                return Err(i.type_err("Reduce of empty array with no initial value"));
            }
            (items[items.len() - 1].clone(), items.len() - 1)
        }
    };
    for k in (0..start).rev() {
        acc = i.call(
            &cb,
            Value::Und,
            vec![acc, items[k].clone(), Value::Num(k as f64), this.clone()],
        )?;
    }
    Ok(acc)
}

fn arr_some(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    let items = arr_clone(this);
    for (k, v) in items.iter().enumerate() {
        let r = i.call(&cb, this_arg.clone(), vec![v.clone(), Value::Num(k as f64), this.clone()])?;
        if i.to_bool(&r) {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

fn arr_every(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    let items = arr_clone(this);
    for (k, v) in items.iter().enumerate() {
        let r = i.call(&cb, this_arg.clone(), vec![v.clone(), Value::Num(k as f64), this.clone()])?;
        if !i.to_bool(&r) {
            return Ok(Value::Bool(false));
        }
    }
    Ok(Value::Bool(true))
}

fn arr_flat(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let depth = i.to_num(&arg(args, 0)).unwrap_or(1.0) as usize;
    let items = arr_clone(this);
    fn rec(i: &Interp, items: Vec<Value>, d: usize, out: &mut Vec<Value>) {
        let _ = i;
        for v in items {
            if d > 0 {
                if let Value::Obj(o) = &v {
                    if let ObjKind::Arr(inner) = &o.borrow().kind {
                        rec(i, inner.clone(), d - 1, out);
                        continue;
                    }
                }
            }
            out.push(v);
        }
    }
    let mut out = Vec::new();
    rec(i, items, depth.max(1), &mut out);
    Ok(Value::Obj(i.new_arr(out)))
}

fn arr_flat_map(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let mapped = arr_map(i, this, args)?;
    arr_flat(i, &mapped, &[Value::Num(1.0)])
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

pub(crate) fn merge_sort(items: &mut [Value], work: &mut Vec<Value>, cmp: &mut dyn FnMut(&Value, &Value) -> std::cmp::Ordering) {
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
    let items = arr_clone(this);
    let n = i.to_num(&arg(args, 0)).unwrap_or(0.0);
    let idx = if n < 0.0 { items.len() as i64 + n as i64 } else { n as i64 };
    if idx < 0 || idx >= items.len() as i64 {
        return Ok(Value::Und);
    }
    Ok(items[idx as usize].clone())
}

fn arr_keys(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let items = arr_clone(this);
    Ok(make_iter(
        i,
        (0..items.len()).map(|k| Value::Num(k as f64)).collect(),
    ))
}

fn arr_values(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    Ok(make_iter(i, arr_clone(this)))
}

fn arr_entries(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let items = arr_clone(this);
    let mut out = Vec::new();
    for (k, v) in items.into_iter().enumerate() {
        out.push(Value::Obj(i.new_arr(vec![Value::Num(k as f64), v])));
    }
    Ok(make_iter(i, out))
}

// ===========================================================================
// natives: string
// ===========================================================================

fn unbox_str(i: &mut Interp, this: &Value) -> Result<String, Value> {
    match this {
        Value::Str(s) => Ok(s.to_string()),
        Value::Obj(o) => {
            if let Some(v) = i.obj_get_raw(o, &PropKey::from_str("$value")) {
                return i.to_str(&v);
            }
            i.to_str(this)
        }
        other => i.to_str(other),
    }
}

fn str_char_at(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let idx = i.to_num(&arg(args, 0)).unwrap_or(0.0) as usize;
    Ok(Value::Str(Rc::from(
        s.chars().nth(idx).map(|c| c.to_string()).unwrap_or_default().as_str(),
    )))
}

fn str_char_code_at(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let idx = i.to_num(&arg(args, 0)).unwrap_or(0.0) as usize;
    // UTF-16 code units
    let units: Vec<u16> = s.encode_utf16().collect();
    Ok(Value::Num(
        units.get(idx).map(|u| *u as f64).unwrap_or(f64::NAN),
    ))
}

fn str_at(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let chars: Vec<char> = s.chars().collect();
    let n = i.to_num(&arg(args, 0)).unwrap_or(0.0);
    let idx = if n < 0.0 { chars.len() as i64 + n as i64 } else { n as i64 };
    if idx < 0 || idx >= chars.len() as i64 {
        return Ok(Value::Und);
    }
    Ok(Value::Str(Rc::from(chars[idx as usize].to_string().as_str())))
}

fn str_index_of(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let needle = i.to_str(&arg(args, 0))?;
    let from = i.to_num(&arg(args, 1)).unwrap_or(0.0).max(0.0) as usize;
    let byte_from = s.char_indices().nth(from).map(|(b, _)| b).unwrap_or(s.len());
    match s[byte_from..].find(&needle) {
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
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let start = clamp_index(i.to_num(&arg(args, 0)).unwrap_or(0.0), len);
    let end = match arg(args, 1) {
        Value::Und => len,
        v => clamp_index(i.to_num(&v).unwrap_or(0.0), len),
    };
    let out: String = if start < end {
        chars[start..end].iter().collect()
    } else {
        String::new()
    };
    Ok(Value::Str(Rc::from(out.as_str())))
}

fn str_substring(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let a = i.to_num(&arg(args, 0)).unwrap_or(0.0).max(0.0) as usize;
    let b = match arg(args, 1) {
        Value::Und => len,
        v => i.to_num(&v).unwrap_or(0.0).max(0.0) as usize,
    };
    let (lo, hi) = (a.min(b).min(len), a.max(b).min(len));
    Ok(Value::Str(Rc::from(
        chars[lo..hi].iter().collect::<String>().as_str(),
    )))
}

fn str_substr(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let start = clamp_index(i.to_num(&arg(args, 0)).unwrap_or(0.0), len);
    let count = match arg(args, 1) {
        Value::Und => len - start,
        v => i.to_num(&v).unwrap_or(0.0).max(0.0) as usize,
    };
    let end = (start + count).min(len);
    Ok(Value::Str(Rc::from(
        chars[start..end].iter().collect::<String>().as_str(),
    )))
}

fn str_to_lower(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    Ok(Value::Str(Rc::from(unbox_str(i, this)?.to_lowercase().as_str())))
}

fn str_to_upper(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    Ok(Value::Str(Rc::from(unbox_str(i, this)?.to_uppercase().as_str())))
}

fn str_trim(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    Ok(Value::Str(Rc::from(unbox_str(i, this)?.trim().to_string().as_str())))
}

fn str_trim_start(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    Ok(Value::Str(Rc::from(
        unbox_str(i, this)?.trim_start().to_string().as_str(),
    )))
}

fn str_trim_end(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    Ok(Value::Str(Rc::from(
        unbox_str(i, this)?.trim_end().to_string().as_str(),
    )))
}

fn str_split(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let a0 = arg(args, 0);
    // Symbol.split protocol (covers RegExp objects)
    if let Value::Obj(_) = &a0 {
        let m = i.get_prop(&a0, &PropKey::Sym(SYM_SPLIT))?;
        if i.is_callable(&m) {
            let sv = Value::Str(Rc::from(s.as_str()));
            return i.call(&m, a0.clone(), vec![sv, arg(args, 1)]);
        }
    }
    let limit = match arg(args, 1) {
        Value::Und => usize::MAX,
        v => i.to_num(&v)?.max(0.0) as usize,
    };
    let parts: Vec<String> = match a0 {
        Value::Und => vec![s],
        v => {
            let sep = i.to_str(&v)?;
            if sep.is_empty() {
                s.chars().map(|c| c.to_string()).collect()
            } else {
                s.split(&sep as &str).map(|p| p.to_string()).collect()
            }
        }
    };
    Ok(Value::Obj(i.new_arr(
        parts
            .into_iter()
            .take(limit)
            .map(|p| Value::Str(Rc::from(p.as_str())))
            .collect(),
    )))
}

fn str_replace(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let a0 = arg(args, 0);
    // Symbol.replace protocol (covers RegExp objects)
    if let Value::Obj(_) = &a0 {
        let m = i.get_prop(&a0, &PropKey::Sym(SYM_REPLACE))?;
        if i.is_callable(&m) {
            let sv = Value::Str(Rc::from(s.as_str()));
            return i.call(&m, a0.clone(), vec![sv, arg(args, 1)]);
        }
    }
    let needle = i.to_str(&a0)?;
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
                        Value::Str(Rc::from(s.as_str())),
                    ],
                )?;
                i.to_str(&r)?
            } else {
                expand_replacement(&i.to_str(&repl_fn)?, &matched, b, &s)
            };
            let mut out = s[..b].to_string();
            out.push_str(&rep);
            out.push_str(&s[b + needle.len()..]);
            Ok(Value::Str(Rc::from(out.as_str())))
        }
        None => Ok(Value::Str(Rc::from(s.as_str()))),
    }
}

fn str_replace_all(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let a0 = arg(args, 0);
    if let Value::Obj(o) = &a0 {
        if let ObjKind::Regex(re) = &o.borrow().kind {
            if !re.global {
                return Err(i.type_err(
                    "String.prototype.replaceAll called with a non-global RegExp argument",
                ));
            }
        }
        let m = i.get_prop(&a0, &PropKey::Sym(SYM_REPLACE))?;
        if i.is_callable(&m) {
            let sv = Value::Str(Rc::from(s.as_str()));
            return i.call(&m, a0.clone(), vec![sv, arg(args, 1)]);
        }
    }
    let needle = i.to_str(&a0)?;
    if needle.is_empty() {
        return Ok(Value::Str(Rc::from(s.as_str())));
    }
    let repl_fn = arg(args, 1);
    if i.is_callable(&repl_fn) {
        let mut out = String::new();
        let mut pos = 0;
        for (b, _) in s.match_indices(&needle) {
            out.push_str(&s[pos..b]);
            let r = i.call(
                &repl_fn,
                Value::Und,
                vec![
                    Value::Str(Rc::from(needle.as_str())),
                    Value::Num(s[..b].chars().count() as f64),
                    Value::Str(Rc::from(s.as_str())),
                ],
            )?;
            out.push_str(&i.to_str(&r)?);
            pos = b + needle.len();
        }
        out.push_str(&s[pos..]);
        return Ok(Value::Str(Rc::from(out.as_str())));
    }
    let rep = i.to_str(&repl_fn)?;
    Ok(Value::Str(Rc::from(
        s.replace(&needle as &str, &expand_replacement(&rep, &needle, 0, &s)).as_str(),
    )))
}

fn str_match(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let a0 = arg(args, 0);
    if let Value::Obj(_) = &a0 {
        let m = i.get_prop(&a0, &PropKey::Sym(SYM_MATCH))?;
        if i.is_callable(&m) {
            let sv = Value::Str(Rc::from(unbox_str(i, this)?.as_str()));
            return i.call(&m, a0.clone(), vec![sv]);
        }
    }
    // non-regex arg → new RegExp(arg), non-global → exec result
    let re = coerce_regexp(i, &a0, "")?;
    let s = unbox_str(i, this)?;
    let chars: Vec<char> = s.chars().collect();
    match re.find(&chars, 0) {
        Some(m) => Ok(match_obj(i, &chars, &re, &m)),
        None => Ok(Value::Null),
    }
}

fn str_match_all(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let a0 = arg(args, 0);
    if let Value::Obj(_) = &a0 {
        let m = i.get_prop(&a0, &PropKey::Sym(SYM_MATCHALL))?;
        if i.is_callable(&m) {
            let sv = Value::Str(Rc::from(unbox_str(i, this)?.as_str()));
            return i.call(&m, a0.clone(), vec![sv]);
        }
    }
    let re = coerce_regexp(i, &a0, "g")?;
    let s = unbox_str(i, this)?;
    let chars: Vec<char> = s.chars().collect();
    let matches = all_matches(&re, &chars);
    let items: Vec<Value> = matches.iter().map(|m| match_obj(i, &chars, &re, m)).collect();
    Ok(make_iter(i, items))
}

fn str_search(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let a0 = arg(args, 0);
    if let Value::Obj(_) = &a0 {
        let m = i.get_prop(&a0, &PropKey::Sym(SYM_SEARCH))?;
        if i.is_callable(&m) {
            let sv = Value::Str(Rc::from(unbox_str(i, this)?.as_str()));
            return i.call(&m, a0.clone(), vec![sv]);
        }
    }
    let re = coerce_regexp(i, &a0, "")?;
    let s = unbox_str(i, this)?;
    let chars: Vec<char> = s.chars().collect();
    match re.find(&chars, 0) {
        Some(m) => Ok(Value::Num(m.start as f64)),
        None => Ok(Value::Num(-1.0)),
    }
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
    let pad = match arg(args, 1) {
        Value::Und => " ".to_string(),
        v => i.to_str(&v)?,
    };
    let cur = s.chars().count();
    if cur >= target || pad.is_empty() {
        return Ok(Value::Str(Rc::from(s.as_str())));
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
    let pad = match arg(args, 1) {
        Value::Und => " ".to_string(),
        v => i.to_str(&v)?,
    };
    let cur = s.chars().count();
    if cur >= target || pad.is_empty() {
        return Ok(Value::Str(Rc::from(s.as_str())));
    }
    let need = target - cur;
    let mut p = String::new();
    while p.chars().count() < need {
        p.push_str(&pad);
    }
    let p: String = p.chars().take(need).collect();
    Ok(Value::Str(Rc::from(format!("{s}{p}").as_str())))
}

fn str_starts_with(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let needle = i.to_str(&arg(args, 0))?;
    let pos = i.to_num(&arg(args, 1)).unwrap_or(0.0).max(0.0) as usize;
    let byte = s.char_indices().nth(pos).map(|(b, _)| b).unwrap_or(s.len());
    Ok(Value::Bool(s[byte..].starts_with(&needle)))
}

fn str_ends_with(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let needle = i.to_str(&arg(args, 0))?;
    Ok(Value::Bool(s.ends_with(&needle)))
}

fn str_includes(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let needle = i.to_str(&arg(args, 0))?;
    Ok(Value::Bool(s.contains(&needle)))
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
    let mut s = unbox_str(i, this)?;
    for a in args {
        s.push_str(&i.to_str(a)?);
    }
    Ok(Value::Str(Rc::from(s.as_str())))
}

fn str_to_string(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    Ok(Value::Str(Rc::from(unbox_str(i, this)?.as_str())))
}

fn str_locale_compare(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    let o = i.to_str(&arg(args, 0))?;
    Ok(Value::Num(match s.cmp(&o) {
        std::cmp::Ordering::Less => -1.0,
        std::cmp::Ordering::Equal => 0.0,
        std::cmp::Ordering::Greater => 1.0,
    }))
}

fn str_iter(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let s = unbox_str(i, this)?;
    Ok(make_iter(
        i,
        s.chars()
            .map(|c| Value::Str(Rc::from(c.to_string().as_str())))
            .collect(),
    ))
}

fn string_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let s = if args.is_empty() {
        String::new()
    } else {
        i.to_str(&arg(args, 0))?
    };
    if matches!(this, Value::Obj(_)) {
        // `new String(x)` → boxed
        if let Value::Obj(o) = this {
            i.set_builtin(o, "$value", Value::Str(Rc::from(s.as_str())));
            i.set_data(o, "length", Value::Num(s.chars().count() as f64));
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
    let len_v = i.get_prop(&strings, &PropKey::from_str("length"))?;
    let n = i.to_num(&len_v)? as usize;
    let mut s = String::new();
    for k in 0..n {
        let seg = i.get_prop(&strings, &PropKey::from_str(&k.to_string()))?;
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
            if let Some(v) = i.obj_get_raw(o, &PropKey::from_str("$value")) {
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
    let s = s.replace("e-", "e-").replacen('e', "e+", 1).replace("e+-", "e-");
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
    Ok(Value::Bool(matches!(arg(args, 0), Value::Num(n) if n.is_nan())))
}

fn number_is_finite(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let _ = i;
    Ok(Value::Bool(matches!(arg(args, 0), Value::Num(n) if n.is_finite())))
}

fn number_is_integer(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let _ = i;
    Ok(Value::Bool(matches!(arg(args, 0), Value::Num(n) if n.is_finite() && n == n.trunc())))
}

fn unbox_bool(i: &mut Interp, this: &Value) -> Result<bool, Value> {
    match this {
        Value::Bool(b) => Ok(*b),
        Value::Obj(o) => {
            if let Some(Value::Bool(b)) = i.obj_get_raw(o, &PropKey::from_str("$value")) {
                return Ok(b);
            }
            Err(i.type_err("Boolean.prototype method called on incompatible receiver"))
        }
        _ => Err(i.type_err("Boolean.prototype method called on incompatible receiver")),
    }
}

fn bool_to_string(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    Ok(Value::Str(Rc::from(unbox_bool(i, this)?.to_string().as_str())))
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
        Value::Sym(_, d) => Ok(Value::Str(Rc::from(d.as_ref()))),
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

fn sym_value_of(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let _ = i;
    Ok(this.clone())
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
        i.set_data(o, "message", Value::Str(Rc::from(msg.as_str())));
        i.set_data(o, "name", Value::Str(Rc::from("Error")));
        i.set_data(o, "stack", Value::Str(Rc::from(format!("Error: {msg}").as_str())));
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
                i.set_data(o, "message", Value::Str(Rc::from(msg.as_str())));
                i.set_data(o, "name", Value::Str(Rc::from($jname)));
                i.set_data(o, "stack", Value::Str(Rc::from(format!("{}: {}", $jname, msg).as_str())));
                return Ok(this.clone());
            }
            let p = i.p.$field.clone();
            Ok(i.err_obj(&p, $jname, &msg))
        }
    };
}
sub_err_ctor!(type_error_ctor, type_error, "TypeError");
sub_err_ctor!(range_error_ctor, range_error, "RangeError");
sub_err_ctor!(syntax_error_ctor, syntax_error, "SyntaxError");
sub_err_ctor!(ref_error_ctor, ref_error, "ReferenceError");

fn err_to_string(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    if let Value::Obj(o) = this {
        let name = i
            .obj_get_raw(o, &PropKey::from_str("name"))
            .map(|v| i.to_str(&v).unwrap_or_else(|_| "Error".into()))
            .unwrap_or_else(|| "Error".into());
        let msg = i
            .obj_get_raw(o, &PropKey::from_str("message"))
            .map(|v| i.to_str(&v).unwrap_or_default())
            .unwrap_or_default();
        if msg.is_empty() {
            return Ok(Value::Str(Rc::from(name.as_str())));
        }
        return Ok(Value::Str(Rc::from(format!("{name}: {msg}").as_str())));
    }
    Ok(Value::Str(Rc::from("Error")))
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
math1!(math_sign, |n: f64| if n.is_nan() { f64::NAN } else { n.signum() });
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
    Ok(Value::Num(to_u32(i.to_num(&arg(args, 0))?).leading_zeros() as f64))
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
                while self.peek().is_some_and(|c| {
                    c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-')
                }) {
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
    match json_ser(i, &v, &indent, 0)? {
        Some(s) => Ok(Value::Str(Rc::from(s.as_str()))),
        None => Ok(Value::Und),
    }
}

fn json_ser(i: &mut Interp, v: &Value, indent: &str, level: usize) -> Result<Option<String>, Value> {
    // replacer via toJSON
    let tj = i.get_prop(v, &PropKey::from_str("toJSON")).unwrap_or(Value::Und);
    let v2 = if i.is_callable(&tj) {
        i.call(&tj, v.clone(), vec![Value::Str(Rc::from(""))])?
    } else {
        v.clone()
    };
    Ok(match &v2 {
        Value::Und | Value::Sym(..) => None,
        Value::Null => Some("null".into()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Num(n) => {
            if n.is_finite() {
                Some(js_num_str(*n))
            } else {
                Some("null".into())
            }
        }
        Value::Str(s) => Some(json_escape(s)),
        Value::Obj(o) => {
            match &o.borrow().kind {
                ObjKind::Arr(items) => {
                    let items = items.clone();
                    let mut parts = Vec::new();
                    for it in &items {
                        parts.push(json_ser(i, it, indent, level + 1)?.unwrap_or_else(|| "null".into()));
                    }
                    Some(if indent.is_empty() {
                        format!("[{}]", parts.join(","))
                    } else {
                        let pad = indent.repeat(level + 1);
                        format!("[\n{}{}\n{}]", pad, parts.join(&format!(",\n{pad}")), indent.repeat(level))
                    })
                }
                ObjKind::Func(_) | ObjKind::Bound { .. } => None,
                _ => {
                    let mut parts = Vec::new();
                    for k in i.own_keys(&v2) {
                        let val = i.get_prop(&v2, &PropKey::from_str(&k))?;
                        if let Some(s) = json_ser(i, &val, indent, level + 1)? {
                            let kv = format!("{}:{}", json_escape(&k), if indent.is_empty() { "" } else { " " });
                            parts.push(format!("{kv}{s}"));
                        }
                    }
                    Some(if indent.is_empty() {
                        format!("{{{}}}", parts.join(","))
                    } else {
                        let pad = indent.repeat(level + 1);
                        format!("{{\n{}{}\n{}}}", pad, parts.join(&format!(",\n{pad}")), indent.repeat(level))
                    })
                }
            }
        }
    })
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
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
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ===========================================================================
// natives: console
// ===========================================================================

fn console_log(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let mut parts = Vec::new();
    for a in args {
        parts.push(i.inspect(a, 0));
    }
    i.output.push(parts.join(" "));
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
                        let then = i.get_prop(&arg(args, 0), &PropKey::from_str("then"))?;
                        let pv = Value::Obj(p.clone());
                        let res_fn = i.native("resolve", promise_resolve_inner);
                        let rej_fn = i.native("reject", promise_reject_inner);
                        let res = Rc::new(RefCell::new(Obj::new(
                            Some(i.p.func.clone()),
                            ObjKind::Bound { target: res_fn, thisv: pv.clone(), extra: vec![] },
                        )));
                        let rej = Rc::new(RefCell::new(Obj::new(
                            Some(i.p.func.clone()),
                            ObjKind::Bound { target: rej_fn, thisv: pv, extra: vec![] },
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
    if let Some(Value::Obj(h)) = p.borrow().props.get(&PropKey::from_str("$handlers")).map(|pr| pr.val.clone()) {
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
                .get(&PropKey::from_str(name))
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
        .get(&PropKey::from_str("$handlers"))
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
        ObjKind::Bound { target: res_fn, thisv: pv.clone(), extra: vec![] },
    )));
    let rej = Rc::new(RefCell::new(Obj::new(
        Some(i.p.func.clone()),
        ObjKind::Bound { target: rej_fn, thisv: pv, extra: vec![] },
    )));
    i.set_builtin(&rec, "resolve", Value::Obj(res));
    i.set_builtin(&rec, "reject", Value::Obj(rej));
    // enqueue
    if let Some(Value::Obj(h)) = p
        .borrow()
        .props
        .get(&PropKey::from_str("$handlers"))
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
                ObjKind::Bound { target: res_fn, thisv: pv.clone(), extra: vec![] },
            )));
            let rej = Rc::new(RefCell::new(Obj::new(
                Some(i.p.func.clone()),
                ObjKind::Bound { target: rej_fn, thisv: pv, extra: vec![] },
            )));
            i.set_builtin(&rec, "resolve", Value::Obj(res));
            i.set_builtin(&rec, "reject", Value::Obj(rej));
            if let Some(Value::Obj(h)) = p
                .borrow()
                .props
                .get(&PropKey::from_str("$handlers"))
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

fn date_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    if !matches!(this, Value::Obj(_)) {
        // Date() called as function → string
        return Ok(Value::Str(Rc::from(
            format!("{} (rowser-js date)", now_millis()).as_str(),
        )));
    }
    let ms = match arg(args, 0) {
        Value::Und => now_millis(),
        Value::Num(n) => n,
        Value::Str(_) => {
            let v = arg(args, 0);
            match date_parse(i, &Value::Und, &[v]) {
                Ok(Value::Num(n)) => n,
                _ => f64::NAN,
            }
        }
        v => i.to_num(&v).unwrap_or(f64::NAN),
    };
    if let Value::Obj(o) = this {
        i.set_builtin(o, "$ms", Value::Num(ms));
    }
    Ok(this.clone())
}

fn date_get_time(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    if let Value::Obj(o) = this {
        if let Some(v) = i.obj_get_raw(o, &PropKey::from_str("$ms")) {
            return Ok(v);
        }
    }
    Ok(Value::Num(f64::NAN))
}

fn date_ms(i: &mut Interp, this: &Value) -> f64 {
    if let Value::Obj(o) = this {
        i.obj_get_raw(o, &PropKey::from_str("$ms"))
            .and_then(|v| match v {
                Value::Num(n) => Some(n),
                _ => None,
            })
            .unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
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
    let ms = date_ms(i, this);
    if !ms.is_finite() {
        return Ok(Value::Num(f64::NAN));
    }
    Ok(Value::Num(f(date_parts(ms))))
}

fn date_to_string(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let ms = date_ms(i, this);
    if !ms.is_finite() {
        return Ok(Value::Str(Rc::from("Invalid Date")));
    }
    let (y, m, d, wd, h, mi, s, _) = date_parts(ms);
    const WD: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MO: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    Ok(Value::Str(Rc::from(format!(
        "{} {} {:02} {} {:02}:{:02}:{:02} GMT+0000 (Coordinated Universal Time)",
        WD[wd as usize],
        MO[(m - 1) as usize],
        d,
        y,
        h,
        mi,
        s
    )
    .as_str())))
}

fn date_to_utc(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let ms = date_ms(i, this);
    if !ms.is_finite() {
        return Ok(Value::Str(Rc::from("Invalid Date")));
    }
    let (y, m, d, wd, h, mi, s, _) = date_parts(ms);
    const WD: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MO: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    Ok(Value::Str(Rc::from(format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        WD[wd as usize],
        d,
        MO[(m - 1) as usize],
        y,
        h,
        mi,
        s
    )
    .as_str())))
}

fn date_to_iso(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let ms = date_ms(i, this);
    if !ms.is_finite() {
        return Ok(Value::Str(Rc::from("Invalid Date")));
    }
    let (y, m, d, _, h, mi, s, msec) = date_parts(ms);
    Ok(Value::Str(Rc::from(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, m, d, h, mi, s, msec
    )
    .as_str())))
}

fn date_to_datestr(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let ms = date_ms(i, this);
    if !ms.is_finite() {
        return Ok(Value::Str(Rc::from("Invalid Date")));
    }
    let (y, m, d, wd, _, _, _, _) = date_parts(ms);
    const WD: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MO: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    Ok(Value::Str(Rc::from(format!(
        "{} {} {:02} {}",
        WD[wd as usize],
        MO[(m - 1) as usize],
        d,
        y
    )
    .as_str())))
}

fn date_to_timestr(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let ms = date_ms(i, this);
    if !ms.is_finite() {
        return Ok(Value::Str(Rc::from("Invalid Date")));
    }
    let (_, _, _, _, h, mi, s, _) = date_parts(ms);
    Ok(Value::Str(Rc::from(format!("{:02}:{:02}:{:02}", h, mi, s).as_str())))
}

fn date_utc(_i: &mut Interp, _t: &Value, a: &[Value]) -> NResult {
    let num = |k: usize, d: f64| match arg(a, k) {
        Value::Num(n) => n,
        _ => d,
    };
    let y = num(0, 0.0) as i64;
    let y = if (0..=99).contains(&y) { 1900 + y } else { y };
    let days = days_from_civil(y, num(1, 0.0) as u32 + 1, num(2, 1.0) as u32);
    let ms = days as f64 * 86_400_000.0
        + num(3, 0.0) * 3_600_000.0
        + num(4, 0.0) * 60_000.0
        + num(5, 0.0) * 1000.0
        + num(6, 0.0);
    Ok(Value::Num(ms))
}

fn date_parse(i: &mut Interp, _t: &Value, a: &[Value]) -> NResult {
    let s = match arg(a, 0) {
        Value::Str(s) => s.to_string(),
        _ => return Ok(Value::Num(f64::NAN)),
    };
    // ISO-ish: YYYY-MM-DD[THH:MM[:SS[.mmm]]][Z]
    let s = s.trim();
    let (dpart, tpart) = s.split_once(['T', ' ']).map(|(d, t)| (d, Some(t))).unwrap_or((s, None));
    let dsegs: Vec<i64> = dpart.split('-').filter_map(|x| x.parse().ok()).collect();
    if dsegs.len() < 3 {
        return Ok(Value::Num(f64::NAN));
    }
    let mut ms = days_from_civil(dsegs[0], dsegs[1] as u32, dsegs[2] as u32) as f64 * 86_400_000.0;
    if let Some(t) = tpart {
        let t = t.trim_end_matches('Z');
        let tsegs: Vec<f64> = t
            .split(':')
            .map(|x| x.parse().unwrap_or(0.0))
            .collect();
        if !tsegs.is_empty() {
            ms += tsegs[0] * 3_600_000.0;
        }
        if tsegs.len() > 1 {
            ms += tsegs[1] * 60_000.0;
        }
        if tsegs.len() > 2 {
            ms += tsegs[2] * 1000.0;
        }
    }
    let _ = i;
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
        Rc::new(RefCell::new(Obj::new(Some(i.p.obj.clone()), ObjKind::Map(Vec::new()))))
    };
    match arg(args, 0) {
        Value::Und | Value::Null => {}
        v => {
            for e in i.iter_values(&v)? {
                let k = i.get_prop(&e, &PropKey::from_str("0"))?;
                let val = i.get_prop(&e, &PropKey::from_str("1"))?;
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
            return Ok(Value::Bool(items.iter().any(|(k2, _)| same_value_zero(k2, &k))));
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
        if let ObjKind::Map(items) = &o.borrow().kind {
            let items = items.clone();
            for (k, v) in items {
                i.call(&cb, this_arg.clone(), vec![v, k, this.clone()])?;
            }
        }
    }
    Ok(Value::Und)
}

fn map_keys(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    if let Value::Obj(o) = this {
        if let ObjKind::Map(items) = &o.borrow().kind {
            let ks: Vec<Value> = items.iter().map(|(k, _)| k.clone()).collect();
            return Ok(make_iter(i, ks));
        }
    }
    Ok(make_iter(i, vec![]))
}

fn map_values(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    if let Value::Obj(o) = this {
        if let ObjKind::Map(items) = &o.borrow().kind {
            let vs: Vec<Value> = items.iter().map(|(_, v)| v.clone()).collect();
            return Ok(make_iter(i, vs));
        }
    }
    Ok(make_iter(i, vec![]))
}

fn map_entries(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    if let Value::Obj(o) = this {
        if let ObjKind::Map(items) = &o.borrow().kind {
            let items = items.clone();
            let mut out = Vec::new();
            for (k, v) in items {
                out.push(Value::Obj(i.new_arr(vec![k, v])));
            }
            return Ok(make_iter(i, out));
        }
    }
    Ok(make_iter(i, vec![]))
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
        Rc::new(RefCell::new(Obj::new(Some(i.p.obj.clone()), ObjKind::Set(Vec::new()))))
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
        if let ObjKind::Set(items) = &o.borrow().kind {
            return Ok(make_iter(i, items.clone()));
        }
    }
    Ok(make_iter(i, vec![]))
}

fn set_entries(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    if let Value::Obj(o) = this {
        if let ObjKind::Set(items) = &o.borrow().kind {
            let items = items.clone();
            let mut out = Vec::new();
            for v in items {
                out.push(Value::Obj(i.new_arr(vec![v.clone(), v])));
            }
            return Ok(make_iter(i, out));
        }
    }
    Ok(make_iter(i, vec![]))
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
            '+' | '-' if seen_e && matches!(bytes.get(k.wrapping_sub(1)), Some('e') | Some('E')) => {}
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
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')') {
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
        if ch.is_ascii_alphanumeric()
            || matches!(ch, '@' | '*' | '_' | '+' | '-' | '.' | '/')
        {
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
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
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
    let k = i.to_propkey(&arg(args, 1))?;
    i.get_prop(&arg(args, 0), &k)
}

fn reflect_set(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let k = i.to_propkey(&arg(args, 1))?;
    i.set_prop(&arg(args, 0), k, arg(args, 2))?;
    Ok(Value::Bool(true))
}

fn reflect_has(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let k = i.to_propkey(&arg(args, 1))?;
    Ok(Value::Bool(i.has_prop(&arg(args, 0), &k)))
}

fn reflect_delete(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let k = i.to_propkey(&arg(args, 1))?;
    Ok(Value::Bool(i.delete_prop(&arg(args, 0), &k)))
}

fn reflect_apply(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let f = arg(args, 0);
    let this_arg = arg(args, 1);
    let list = to_elems(i, &arg(args, 2))?;
    i.call(&f, this_arg, list)
}

fn reflect_construct(i: &mut Interp, _t: &Value, args: &[Value]) -> NResult {
    let f = arg(args, 0);
    let list = to_elems(i, &arg(args, 1))?;
    i.construct(&f, list, None)
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

/// Coerce a JS value into a compiled Regex (regex objects pass through,
/// anything else is treated as a pattern string compiled with `flags`).
fn coerce_regexp(i: &mut Interp, v: &Value, flags: &str) -> Result<Rc<Regex>, Value> {
    if let Some(re) = re_of(v) {
        return Ok(re);
    }
    let pat = match v {
        Value::Und => String::new(),
        v => i.to_str(v)?,
    };
    match Regex::compile(&pat, flags) {
        Ok(re) => Ok(re),
        Err(e) => Err(i.syntax_err(format!("Invalid regular expression: /{pat}/: {e}"))),
    }
}

fn chars_str(chars: &[char], a: usize, b: usize) -> Value {
    let s: String = chars[a..b].iter().collect();
    Value::Str(Rc::from(s.as_str()))
}

fn chars_string(chars: &[char]) -> String {
    chars.iter().collect()
}

/// exec result: array [m0, g1..gn] + index/input/groups props.
fn match_obj(i: &mut Interp, chars: &[char], re: &Regex, m: &RxMatch) -> Value {
    let mut items = vec![chars_str(chars, m.start, m.end)];
    for g in 1..=re.n_groups {
        items.push(match m.caps.get(g).copied().flatten() {
            Some((a, b)) => chars_str(chars, a, b),
            None => Value::Und,
        });
    }
    let arr = i.new_arr(items);
    i.set_data(&arr, "index", Value::Num(m.start as f64));
    i.set_data(&arr, "input", Value::Str(Rc::from(chars_string(chars).as_str())));
    if re.names.is_empty() {
        i.set_data(&arr, "groups", Value::Und);
    } else {
        let groups = groups_obj(i, chars, re, m);
        i.set_data(&arr, "groups", groups);
    }
    Value::Obj(arr)
}

fn groups_obj(i: &mut Interp, chars: &[char], re: &Regex, m: &RxMatch) -> Value {
    let g = i.new_obj();
    for (name, idx) in &re.names {
        let v = match m.caps.get(*idx).copied().flatten() {
            Some((a, b)) => chars_str(chars, a, b),
            None => Value::Und,
        };
        i.set_data(&g, name, v);
    }
    Value::Obj(g)
}

/// Run exec honoring lastIndex semantics for g/y regexes.
fn regexp_run(
    i: &mut Interp,
    this: &Value,
    re: &Regex,
    chars: &[char],
) -> Result<Option<RxMatch>, Value> {
    let use_last = re.global || re.sticky;
    let start = if use_last {
        let lv = i.get_prop(this, &PropKey::from_str("lastIndex"))?;
        let n = i.to_num(&lv)?;
        if n.is_finite() && n >= 0.0 {
            n as usize
        } else {
            0
        }
    } else {
        0
    };
    let m = re.exec(chars, start);
    if use_last {
        let v = match &m {
            Some(mm) => Value::Num(mm.end as f64),
            None => Value::Num(0.0),
        };
        i.set_prop(this, PropKey::from_str("lastIndex"), v)?;
    }
    Ok(m)
}

/// All successive matches for global-style loops (with empty-match advance).
fn all_matches(re: &Regex, chars: &[char]) -> Vec<RxMatch> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    loop {
        match re.find(chars, pos) {
            Some(m) => {
                let empty = m.end == m.start;
                pos = m.end + usize::from(empty);
                out.push(m);
                if out.len() >= 1_000_000 {
                    break;
                }
            }
            None => break,
        }
    }
    out
}

fn regexp_ctor(i: &mut Interp, _this: &Value, args: &[Value]) -> NResult {
    let a0 = arg(args, 0);
    let (source, flags) = if let Some(re) = re_of(&a0) {
        let f = match arg(args, 1) {
            Value::Und => re.flags.clone(),
            v => i.to_str(&v)?,
        };
        (re.source.clone(), f)
    } else {
        let s = match a0 {
            Value::Und => String::new(),
            v => i.to_str(&v)?,
        };
        let f = match arg(args, 1) {
            Value::Und => String::new(),
            v => i.to_str(&v)?,
        };
        (s, f)
    };
    i.make_regexp(&source, &flags)
}

fn re_exec(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let Some(re) = re_of(this) else {
        return Err(i.type_err("RegExp.prototype.exec called on incompatible receiver"));
    };
    let s = i.to_str(&arg(args, 0))?;
    let chars: Vec<char> = s.chars().collect();
    match regexp_run(i, this, &re, &chars)? {
        Some(m) => Ok(match_obj(i, &chars, &re, &m)),
        None => Ok(Value::Null),
    }
}

fn re_test(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let Some(re) = re_of(this) else {
        return Err(i.type_err("RegExp.prototype.test called on incompatible receiver"));
    };
    let s = i.to_str(&arg(args, 0))?;
    let chars: Vec<char> = s.chars().collect();
    Ok(Value::Bool(regexp_run(i, this, &re, &chars)?.is_some()))
}

fn re_to_string(i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    let Some(re) = re_of(this) else {
        // spec: generic — read .source/.flags
        let src = i.get_prop(this, &PropKey::from_str("source"))?;
        let fl = i.get_prop(this, &PropKey::from_str("flags"))?;
        let s = format!("/{}/{}", i.to_str(&src)?, i.to_str(&fl)?);
        return Ok(Value::Str(Rc::from(s.as_str())));
    };
    let src = if re.source.is_empty() {
        "(?:)".to_string()
    } else {
        re.source.clone()
    };
    Ok(Value::Str(Rc::from(
        format!("/{src}/{}", re.flags).as_str(),
    )))
}

/// `re.compile(pat, flags)` — legacy: recompiles in place.
fn re_compile(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let a0 = arg(args, 0);
    let (source, flags) = if let Some(re) = re_of(&a0) {
        let f = match arg(args, 1) {
            Value::Und => re.flags.clone(),
            v => i.to_str(&v)?,
        };
        (re.source.clone(), f)
    } else {
        (i.to_str(&a0)?, i.to_str(&arg(args, 1)).unwrap_or_default())
    };
    let re = match Regex::compile(&source, &flags) {
        Ok(r) => r,
        Err(e) => return Err(i.syntax_err(format!("Invalid regular expression: {e}"))),
    };
    if let Value::Obj(o) = this {
        o.borrow_mut().kind = ObjKind::Regex(re);
    }
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

fn re_get_source(_i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    match re_of(this) {
        Some(re) => Ok(Value::Str(Rc::from(
            if re.source.is_empty() {
                "(?:)".to_string()
            } else {
                re.source.clone()
            }
            .as_str(),
        ))),
        None => Ok(Value::Und),
    }
}

fn re_get_flags(_i: &mut Interp, this: &Value, _a: &[Value]) -> NResult {
    match re_of(this) {
        Some(re) => Ok(Value::Str(Rc::from(re.flags.as_str()))),
        None => Ok(Value::Und),
    }
}

// -- Symbol.* protocol methods --------------------------------------------------

fn re_sym_match(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let Some(re) = re_of(this) else {
        return Err(i.type_err("Symbol.match called on non-RegExp"));
    };
    let s = i.to_str(&arg(args, 0))?;
    let chars: Vec<char> = s.chars().collect();
    if re.global {
        // collect all via lastIndex-driven exec
        i.set_prop(this, PropKey::from_str("lastIndex"), Value::Num(0.0))?;
        let mut out = Vec::new();
        loop {
            match regexp_run(i, this, &re, &chars)? {
                Some(m) => {
                    let empty = m.end == m.start;
                    out.push(chars_str(&chars, m.start, m.end));
                    if empty {
                        let _ = i.set_prop(
                            this,
                            PropKey::from_str("lastIndex"),
                            Value::Num(m.end as f64 + 1.0),
                        );
                    }
                }
                None => break,
            }
            if out.len() >= 1_000_000 {
                break;
            }
        }
        if out.is_empty() {
            Ok(Value::Null)
        } else {
            Ok(Value::Obj(i.new_arr(out)))
        }
    } else {
        match regexp_run(i, this, &re, &chars)? {
            Some(m) => Ok(match_obj(i, &chars, &re, &m)),
            None => Ok(Value::Null),
        }
    }
}

fn re_sym_match_all(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let Some(re) = re_of(this) else {
        return Err(i.type_err("Symbol.matchAll called on non-RegExp"));
    };
    if !re.global {
        return Err(i.type_err(
            "String.prototype.matchAll called with a non-global RegExp argument",
        ));
    }
    let s = i.to_str(&arg(args, 0))?;
    let chars: Vec<char> = s.chars().collect();
    let matches = all_matches(&re, &chars);
    let items: Vec<Value> = matches
        .iter()
        .map(|m| match_obj(i, &chars, &re, m))
        .collect();
    Ok(make_iter(i, items))
}

fn re_sym_search(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let Some(re) = re_of(this) else {
        return Err(i.type_err("Symbol.search called on non-RegExp"));
    };
    let s = i.to_str(&arg(args, 0))?;
    let chars: Vec<char> = s.chars().collect();
    i.set_prop(this, PropKey::from_str("lastIndex"), Value::Num(0.0))?;
    match re.find(&chars, 0) {
        Some(m) => Ok(Value::Num(m.start as f64)),
        None => Ok(Value::Num(-1.0)),
    }
}

/// Substitution expansion for regex replaces: `$$` `$&` `` $` `` `$'` `$n`
/// `$nn` `$<name>`.
fn expand_replacement_rx(
    rep: &str,
    m: &RxMatch,
    chars: &[char],
    re: &Regex,
) -> String {
    let mut out = String::new();
    let cs: Vec<char> = rep.chars().collect();
    let mut j = 0;
    while j < cs.len() {
        if cs[j] != '$' {
            out.push(cs[j]);
            j += 1;
            continue;
        }
        let next = cs.get(j + 1).copied();
        match next {
            Some('$') => {
                out.push('$');
                j += 2;
            }
            Some('&') => {
                out.push_str(&chars_string(&chars[m.start..m.end]));
                j += 2;
            }
            Some('`') => {
                out.push_str(&chars_string(&chars[..m.start]));
                j += 2;
            }
            Some('\'') => {
                out.push_str(&chars_string(&chars[m.end..]));
                j += 2;
            }
            Some('<') => {
                // $<name>
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
                if closed && re.names.iter().any(|(n, _)| *n == name) {
                    if let Some(idx) = re.names.iter().find(|(n, _)| *n == name).map(|(_, i)| *i) {
                        if let Some(Some((a, b))) = m.caps.get(idx) {
                            out.push_str(&chars_string(&chars[*a..*b]));
                        }
                    }
                    j = k + 1;
                } else {
                    out.push('$');
                    j += 1;
                }
            }
            Some(d) if d.is_ascii_digit() => {
                // $n or $nn — prefer 2-digit if it names a real group
                let d1 = d.to_digit(10).unwrap() as usize;
                let d2 = cs
                    .get(j + 2)
                    .and_then(|c| c.to_digit(10))
                    .map(|x| d1 * 10 + x as usize);
                let (g, len) = match d2 {
                    Some(g) if g > 0 && g <= re.n_groups => (g, 2),
                    _ if d1 > 0 && d1 <= re.n_groups => (d1, 1),
                    _ => {
                        out.push('$');
                        j += 1;
                        continue;
                    }
                };
                if let Some(Some((a, b))) = m.caps.get(g) {
                    out.push_str(&chars_string(&chars[*a..*b]));
                }
                j += 1 + len;
            }
            _ => {
                out.push('$');
                j += 1;
            }
        }
    }
    out
}

/// Shared regex-replace core: run `matches`, splice in replacements.
fn regex_replace_core(
    i: &mut Interp,
    re: &Regex,
    chars: &[char],
    s: &str,
    matches: &[RxMatch],
    repl: &Value,
) -> Result<String, Value> {
    let mut out = String::new();
    let mut last = 0usize;
    let callable = i.is_callable(repl);
    let rep_str = if !callable {
        Some(i.to_str(repl)?)
    } else {
        None
    };
    for m in matches {
        out.push_str(&chars_string(&chars[last..m.start]));
        let rep = if callable {
            let mut cargs: Vec<Value> = vec![chars_str(chars, m.start, m.end)];
            for g in 1..=re.n_groups {
                cargs.push(match m.caps.get(g).copied().flatten() {
                    Some((a, b)) => chars_str(chars, a, b),
                    None => Value::Und,
                });
            }
            cargs.push(Value::Num(m.start as f64));
            cargs.push(Value::Str(Rc::from(s)));
            if !re.names.is_empty() {
                cargs.push(groups_obj(i, chars, re, m));
            }
            let r = i.call(repl, Value::Und, cargs)?;
            i.to_str(&r)?
        } else {
            expand_replacement_rx(rep_str.as_deref().unwrap_or_default(), m, chars, re)
        };
        out.push_str(&rep);
        last = m.end;
    }
    out.push_str(&chars_string(&chars[last..]));
    Ok(out)
}

fn re_sym_replace(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let Some(re) = re_of(this) else {
        return Err(i.type_err("Symbol.replace called on non-RegExp"));
    };
    let s = i.to_str(&arg(args, 0))?;
    let repl = arg(args, 1);
    let chars: Vec<char> = s.chars().collect();
    let matches = if re.global {
        all_matches(&re, &chars)
    } else {
        match re.find(&chars, 0) {
            Some(m) => vec![m],
            None => Vec::new(),
        }
    };
    let out = regex_replace_core(i, &re, &chars, &s, &matches, &repl)?;
    Ok(Value::Str(Rc::from(out.as_str())))
}

fn re_sym_split(i: &mut Interp, this: &Value, args: &[Value]) -> NResult {
    let Some(re) = re_of(this) else {
        return Err(i.type_err("Symbol.split called on non-RegExp"));
    };
    let s = i.to_str(&arg(args, 0))?;
    let chars: Vec<char> = s.chars().collect();
    let lim = match arg(args, 1) {
        Value::Und => usize::MAX,
        v => i.to_num(&v)?.max(0.0) as usize,
    };
    let mut out: Vec<Value> = Vec::new();
    if chars.is_empty() {
        if re.try_at(&chars, 0).is_none() {
            out.push(Value::Str(Rc::from(s.as_str())));
        }
        return Ok(Value::Obj(i.new_arr(out)));
    }
    // spec algorithm: sticky exec anchored at searchIndex
    let (mut lme, mut si) = (0usize, 0usize); // lastMatchEnd, searchIndex
    while si < chars.len() {
        match re.try_at(&chars, si) {
            None => si += 1,
            Some(m) => {
                let e = m.end.min(chars.len());
                if e == lme {
                    // empty match at the boundary — skip one char
                    si += 1;
                    continue;
                }
                out.push(chars_str(&chars, lme, si));
                for g in 1..=re.n_groups {
                    out.push(match m.caps.get(g).copied().flatten() {
                        Some((a, b)) => chars_str(&chars, a, b),
                        None => Value::Und,
                    });
                }
                if out.len() >= lim {
                    break;
                }
                lme = e;
                si = e;
            }
        }
    }
    out.push(chars_str(&chars, lme, chars.len()));
    out.truncate(lim);
    Ok(Value::Obj(i.new_arr(out)))
}
