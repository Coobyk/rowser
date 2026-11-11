//! dom_bridge — bind a `rowser_dom::Document` into a JS `Context`.
//!
//! A `DomContext` owns a `Context` whose `interp.ext` carries `DomState`:
//! the document(s), node→wrapper map, per-tag prototypes, event/timer/raf/
//! microtask queues and in-memory storage. DOM host objects are plain JS
//! objects with hidden `"\u{0}…"` props holding `(did, nid)` handles into
//! `DomState.docs`.
//!
//! ```
//! let doc = rowser_html::parse("<div id=root></div>");
//! let mut dc = rowser_js::dom_bridge::DomContext::new(doc, "file:///x.html");
//! dc.eval("document.getElementById('root').innerHTML = '<p>hi</p>'").unwrap();
//! let doc = dc.take_document();
//! ```

use crate::interp::*;
use crate::{builtins, Context, Error};
use rowser_dom::{Document, NodeId, NodeKind};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

type NR = Result<Value, Value>;

const FRAG_TAG: &str = "#fragment";

// Hidden props — `\u{0}` can never appear in a JS identifier, so these are
// invisible to scripts and immune to name collisions.
const K_DID: &str = "\u{0}did";
const K_NID: &str = "\u{0}nid";
const K_EL: &str = "\u{0}el"; // back-reference to the element object
const K_LISTEN: &str = "\u{0}lsn"; // {type: [fns], "\u{1}type": [capture fns]}
const K_STYLE: &str = "\u{0}sty";
const K_DECLS: &str = "\u{0}dcl";
const K_DATASET: &str = "\u{0}dst";
const K_CLASSLIST: &str = "\u{0}cl";
const K_STORE: &str = "\u{0}stw";
const K_CB: &str = "\u{0}cb";
const K_PEER: &str = "\u{0}peer";
const K_STOP: &str = "\u{0}sp";
const K_STOPIMM: &str = "\u{0}si";
const K_NS: &str = "\u{0}ns";

fn pk(s: &str) -> PropKey {
    PropKey::from_str(s)
}

fn arg(args: &[Value], n: usize) -> Value {
    args.get(n).cloned().unwrap_or(Value::Und)
}

fn obj(proto: Option<ObjRef>) -> ObjRef {
    Rc::new(RefCell::new(Obj::new(proto, ObjKind::Ord)))
}

// ---------------------------------------------------------------------------
// DomState
// ---------------------------------------------------------------------------

struct TimerJob {
    id: u64,
    cb: Value,
    args: Vec<Value>,
    interval: bool,
}

/// Embedder state living in `interp.ext`. Natives reach it via `with_ds`.
pub struct DomState {
    /// docs[0] is the page document; DOMParser/innerHTML-helpers append more.
    docs: Vec<Document>,
    url: String,
    t0: std::time::Instant,
    /// (did, nid) → JS wrapper (identity-preserving).
    nodes: HashMap<(u32, NodeId), ObjRef>,
    /// tag → per-tag prototype (chains to p_html/p_svg).
    tag_protos: HashMap<String, ObjRef>,
    /// nodes created with the SVG namespace.
    svg_nodes: std::collections::HashSet<(u32, NodeId)>,
    doc_objs: HashMap<u32, ObjRef>,
    p_html: ObjRef,
    p_svg: ObjRef,
    p_frag: ObjRef,
    p_doc: ObjRef,
    p_char: ObjRef,
    p_event: ObjRef,
    p_style: ObjRef,
    p_classlist: ObjRef,
    p_storage: ObjRef,
    p_compstyle: ObjRef,
    timers: VecDeque<TimerJob>,
    next_timer: u64,
    raf: VecDeque<(u64, Value)>,
    next_raf: u64,
    microtasks: VecDeque<(Value, Vec<Value>)>,
    port_msgs: VecDeque<(ObjRef, Value)>,
    storage_local: Vec<(String, String)>,
    storage_session: Vec<(String, String)>,
    focused: Option<(u32, NodeId)>,
    location: Option<ObjRef>,
}

/// Run `f` with the `DomState` out of `interp.ext`. `f` must not call back
/// into JS (no `i.call`/`get_prop`) — take data out, drop, then call JS.
fn with_ds<R>(i: &mut Interp, f: impl FnOnce(&mut DomState) -> R) -> R {
    let mut st = i
        .ext
        .take()
        .expect("rowser-js dom_bridge: ext missing (not a DomContext)");
    let ds = st
        .downcast_mut::<DomState>()
        .expect("rowser-js dom_bridge: ext is not a DomState");
    let r = f(ds);
    i.ext = Some(st);
    r
}

// ---------------------------------------------------------------------------
// node → JS object
// ---------------------------------------------------------------------------

/// The (doc-index, node) a wrapper points at, if it is a DOM wrapper.
fn node_ref(i: &Interp, v: &Value) -> Option<(u32, NodeId)> {
    let Value::Obj(o) = v else { return None };
    let d = i.obj_get_raw(o, &pk(K_DID));
    let n = i.obj_get_raw(o, &pk(K_NID));
    match (d, n) {
        (Some(Value::Num(d)), Some(Value::Num(n))) => Some((d as u32, n as usize)),
        _ => None,
    }
}

/// Element-object prototype for a tag: per-tag proto → HTMLElement.prototype
/// (or SVGElement.prototype for svg/NS-created nodes) → Element → Node → Object.
fn proto_for(ds: &mut DomState, did: u32, nid: NodeId) -> ObjRef {
    let doc = &ds.docs[did as usize];
    let Some(tag) = doc.tag(nid).map(|t| t.to_string()) else {
        return ds.p_char.clone();
    };
    if tag == FRAG_TAG {
        return ds.p_frag.clone();
    }
    let svg = tag == "svg" || ds.svg_nodes.contains(&(did, nid));
    let base = if svg {
        ds.p_svg.clone()
    } else {
        ds.p_html.clone()
    };
    if let Some(p) = ds.tag_protos.get(&tag) {
        return p.clone();
    }
    let p = obj(Some(base));
    ds.tag_protos.insert(tag, p.clone());
    p
}

/// Identity-preserving wrapper: same node → same JS object.
fn wrap_node(i: &mut Interp, did: u32, nid: NodeId) -> Value {
    if let NodeKind::Document = with_ds(i, |ds| ds.docs[did as usize].kind(nid).clone()) {
        return doc_obj(i, did);
    }
    if let Some(o) = with_ds(i, |ds| ds.nodes.get(&(did, nid)).cloned()) {
        return Value::Obj(o);
    }
    let proto = with_ds(i, |ds| proto_for(ds, did, nid));
    let o = obj(Some(proto));
    i.set_builtin(&o, K_DID, Value::Num(did as f64));
    i.set_builtin(&o, K_NID, Value::Num(nid as f64));
    with_ds(i, |ds| {
        ds.nodes.insert((did, nid), o.clone());
    });
    Value::Obj(o)
}

/// The `document` object for doc `did`.
fn doc_obj(i: &mut Interp, did: u32) -> Value {
    if let Some(o) = with_ds(i, |ds| ds.doc_objs.get(&did).cloned()) {
        return Value::Obj(o);
    }
    let (proto, root) = with_ds(i, |ds| (ds.p_doc.clone(), ds.docs[did as usize].root));
    let o = obj(Some(proto));
    i.set_builtin(&o, K_DID, Value::Num(did as f64));
    i.set_builtin(&o, K_NID, Value::Num(root as f64));
    i.set_builtin(&o, "readyState", Value::Str(Rc::from("complete")));
    i.set_builtin(&o, "compatMode", Value::Str(Rc::from("CSS1Compat")));
    i.set_builtin(&o, "characterSet", Value::Str(Rc::from("UTF-8")));
    i.set_builtin(&o, "charset", Value::Str(Rc::from("UTF-8")));
    i.set_builtin(&o, "contentType", Value::Str(Rc::from("text/html")));
    i.set_builtin(&o, "designMode", Value::Str(Rc::from("off")));
    let url = with_ds(i, |ds| ds.url.clone());
    i.set_builtin(&o, "URL", Value::Str(Rc::from(url.as_str())));
    i.set_builtin(&o, "documentURI", Value::Str(Rc::from(url.as_str())));
    i.set_builtin(&o, "cookie", Value::Str(Rc::from("")));
    i.set_builtin(&o, "referrer", Value::Str(Rc::from("")));
    let g = Value::Obj(i.p.global.clone());
    i.set_builtin(&o, "defaultView", g);
    let loc = location_obj(i);
    i.set_builtin(&o, "location", loc);
    i.set_builtin(&o, "doctype", Value::Null);
    // registration maps — let wrap_node(root) resolve here too
    with_ds(i, |ds| {
        ds.doc_objs.insert(did, o.clone());
        ds.nodes.insert((did, root), o.clone());
    });
    Value::Obj(o)
}

// ---------------------------------------------------------------------------
// raw DOM helpers (operate on pub arena fields; no interp)
// ---------------------------------------------------------------------------

fn fix_siblings(doc: &mut Document, parent: NodeId) {
    let kids = doc.nodes[parent].children.clone();
    for (k, c) in kids.iter().enumerate() {
        doc.nodes[*c].sibling_index = k;
    }
}

fn detach(doc: &mut Document, id: NodeId) {
    if let Some(p) = doc.nodes[id].parent {
        doc.nodes[p].children.retain(|&c| c != id);
        fix_siblings(doc, p);
    }
    doc.nodes[id].parent = None;
    doc.nodes[id].sibling_index = usize::MAX;
}

/// Insert `child` under `parent` at `pos` (detaching it first).
fn insert_at(doc: &mut Document, parent: NodeId, child: NodeId, pos: usize) {
    detach(doc, child);
    let pos = pos.min(doc.nodes[parent].children.len());
    doc.nodes[parent].children.insert(pos, child);
    doc.nodes[child].parent = Some(parent);
    fix_siblings(doc, parent);
}

fn pos_in_parent(doc: &Document, id: NodeId) -> Option<(NodeId, usize)> {
    let p = doc.nodes[id].parent?;
    let pos = doc.nodes[p].children.iter().position(|&c| c == id)?;
    Some((p, pos))
}

/// Insert node-or-fragment under `parent` at `pos` (fragment splices kids).
fn insert_maybe_frag(doc: &mut Document, parent: NodeId, child: NodeId, pos: usize) {
    if doc.tag(child) == Some(FRAG_TAG) {
        let kids = std::mem::take(&mut doc.nodes[child].children);
        for (off, k) in kids.into_iter().enumerate() {
            doc.nodes[k].parent = None;
            insert_at(doc, parent, k, pos + off);
        }
        doc.nodes[child].parent = None;
        doc.nodes[child].sibling_index = usize::MAX;
    } else {
        insert_at(doc, parent, child, pos);
    }
}

fn remove_children(doc: &mut Document, id: NodeId) {
    let kids = std::mem::take(&mut doc.nodes[id].children);
    for c in kids {
        doc.nodes[c].parent = None;
        doc.nodes[c].sibling_index = usize::MAX;
    }
}

fn clone_subtree(dst: &mut Document, src: &Document, id: NodeId) -> NodeId {
    let n = match src.kind(id) {
        NodeKind::Document => return dst.root,
        NodeKind::Element { tag, attrs } => dst.create_element(
            tag,
            attrs.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        ),
        NodeKind::Text(t) => dst.create_text(t),
        NodeKind::Comment(c) => dst.create_comment(c),
    };
    for &c in &src.nodes[id].children {
        let cc = clone_subtree(dst, src, c);
        dst.append(n, cc);
    }
    n
}

/// Deep-clone a subtree within one document.
fn clone_subtree_inplace(doc: &mut Document, id: NodeId) -> NodeId {
    let kind = doc.nodes[id].kind.clone();
    let n = match kind {
        NodeKind::Document => return doc.root,
        NodeKind::Element { tag, attrs } => doc.create_element(&tag, attrs.into_iter().collect()),
        NodeKind::Text(t) => doc.create_text(&t),
        NodeKind::Comment(c) => doc.create_comment(&c),
    };
    let kids = doc.nodes[id].children.clone();
    for c in kids {
        let cc = clone_subtree_inplace(doc, c);
        doc.append(n, cc);
    }
    n
}

fn attr_set(doc: &mut Document, id: NodeId, name: &str, val: &str) {
    if let NodeKind::Element { attrs, .. } = &mut doc.nodes[id].kind {
        attrs.insert(name.to_string(), val.to_string());
    }
}

fn attr_remove(doc: &mut Document, id: NodeId, name: &str) {
    if let NodeKind::Element { attrs, .. } = &mut doc.nodes[id].kind {
        attrs.remove(name);
    }
}

/// `innerHTML` get / `outerHTML` — serialize a subtree back to markup.
fn serialize(doc: &Document, id: NodeId, out: &mut String) {
    match doc.kind(id) {
        NodeKind::Document => serialize_children(doc, id, out),
        NodeKind::Text(t) => escape_text(t, out),
        NodeKind::Comment(c) => {
            out.push_str("<!--");
            out.push_str(c);
            out.push_str("-->");
        }
        NodeKind::Element { tag, attrs } => {
            if tag == FRAG_TAG {
                return serialize_children(doc, id, out);
            }
            out.push('<');
            out.push_str(tag);
            let mut names: Vec<&String> = attrs.keys().collect();
            names.sort();
            for k in names {
                out.push(' ');
                out.push_str(k);
                out.push_str("=\"");
                escape_attr(&attrs[k], out);
                out.push('"');
            }
            out.push('>');
            const VOID: &[&str] = &[
                "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta",
                "param", "source", "track", "wbr",
            ];
            if !VOID.contains(&tag.as_str()) {
                serialize_children(doc, id, out);
                out.push_str("</");
                out.push_str(tag);
                out.push('>');
            }
        }
    }
}

fn serialize_children(doc: &Document, id: NodeId, out: &mut String) {
    for &c in &doc.nodes[id].children {
        serialize(doc, c, out);
    }
}

fn escape_text(t: &str, out: &mut String) {
    for c in t.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

fn escape_attr(t: &str, out: &mut String) {
    for c in t.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

/// `innerHTML` set — parse markup and graft its body's children under `id`.
fn set_inner_html(ds: &mut DomState, did: u32, id: NodeId, html: &str) {
    let parsed = rowser_html::parse(html);
    let src = parsed.find_deep(parsed.root, "body").unwrap_or(parsed.root);
    let doc = &mut ds.docs[did as usize];
    remove_children(doc, id);
    graft_children(doc, &parsed, src, id);
}

fn graft_children(doc: &mut Document, parsed: &Document, src_parent: NodeId, dst_parent: NodeId) {
    for &c in &parsed.nodes[src_parent].children {
        let n = graft_node(doc, parsed, c);
        doc.append(dst_parent, n);
    }
}

fn graft_node(doc: &mut Document, parsed: &Document, id: NodeId) -> NodeId {
    match parsed.kind(id) {
        NodeKind::Document => {
            let frag = doc.create_element(FRAG_TAG, vec![]);
            graft_children(doc, parsed, id, frag);
            frag
        }
        NodeKind::Element { tag, attrs } => {
            let n = doc.create_element(
                tag,
                attrs.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            );
            graft_children(doc, parsed, id, n);
            n
        }
        NodeKind::Text(t) => doc.create_text(t),
        NodeKind::Comment(c) => doc.create_comment(c),
    }
}

// ---------------------------------------------------------------------------
// events
// ---------------------------------------------------------------------------

/// `__lsn` map on an object: {"type": [bubble fns], "\u{1}type": [capture fns]}.
fn listeners(i: &mut Interp, o: &ObjRef, create: bool) -> Option<ObjRef> {
    if let Some(Value::Obj(m)) = i.obj_get_raw(o, &pk(K_LISTEN)) {
        return Some(m);
    }
    if !create {
        return None;
    }
    let m = i.new_obj();
    i.set_builtin(o, K_LISTEN, Value::Obj(m.clone()));
    Some(m)
}

fn listener_fns(i: &mut Interp, o: &ObjRef, ty: &str, capture: bool) -> Vec<Value> {
    let Some(m) = listeners(i, o, false) else {
        return Vec::new();
    };
    let key = if capture {
        format!("\u{1}{ty}")
    } else {
        ty.to_string()
    };
    if let Some(Value::Obj(a)) = i.obj_get_raw(&m, &pk(&key)) {
        if let ObjKind::Arr(items) = &a.borrow().kind {
            return items.clone();
        }
    }
    Vec::new()
}

fn el_add_event_listener(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Value::Obj(o) = this else {
        return Ok(Value::Und);
    };
    let ty = i.to_str(&arg(args, 0))?;
    let f = arg(args, 1);
    if !i.is_callable(&f) {
        return Ok(Value::Und);
    }
    let capture = i.to_bool(&arg(args, 2));
    let Some(m) = listeners(i, o, true) else {
        return Ok(Value::Und);
    };
    let key = if capture {
        format!("\u{1}{ty}")
    } else {
        ty.to_string()
    };
    let arr = match i.obj_get_raw(&m, &pk(&key)) {
        Some(Value::Obj(a)) => a,
        _ => {
            let a = i.new_arr(Vec::new());
            i.set_builtin(&m, &key, Value::Obj(a.clone()));
            a
        }
    };
    if let ObjKind::Arr(items) = &mut arr.borrow_mut().kind {
        items.push(f);
    }
    Ok(Value::Und)
}

fn el_remove_event_listener(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Value::Obj(o) = this else {
        return Ok(Value::Und);
    };
    let ty = i.to_str(&arg(args, 0))?;
    let f = arg(args, 1);
    let capture = i.to_bool(&arg(args, 2));
    let Some(m) = listeners(i, o, false) else {
        return Ok(Value::Und);
    };
    let key = if capture {
        format!("\u{1}{ty}")
    } else {
        ty.to_string()
    };
    if let Some(Value::Obj(a)) = i.obj_get_raw(&m, &pk(&key)) {
        if let ObjKind::Arr(items) = &mut a.borrow_mut().kind {
            // find first equal fn
            let mut rm = None;
            for (k, it) in items.iter().enumerate() {
                if i.same_value(it, &f).unwrap_or(false) {
                    rm = Some(k);
                    break;
                }
            }
            if let Some(k) = rm {
                items.remove(k);
            }
        }
    }
    Ok(Value::Und)
}

fn el_dispatch_event(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let ev = arg(args, 0);
    if !matches!(ev, Value::Obj(_)) {
        return Err(i.type_err("dispatchEvent: argument is not an Event"));
    }
    let tyv = i.get_prop(&ev, &pk("type"))?;
    let ty = i.to_str(&tyv)?;
    if ty.is_empty() {
        return Err(i.type_err("dispatchEvent: event has no type"));
    }
    // seed target/srcElement
    let cur_target = i.get_prop(&ev, &pk("target"))?;
    if matches!(cur_target, Value::Und | Value::Null) {
        i.set_prop(&ev, pk("target"), this.clone())?;
        i.set_prop(&ev, pk("srcElement"), this.clone())?;
    }
    run_event_path(i, &this, &ev)
}

/// The capture→target→bubble dispatch walk shared by `dispatchEvent` and
/// embedder-driven events (clicks). Returns `false` when the event was
/// cancelled (preventDefault).
fn run_event_path(i: &mut Interp, this: &Value, ev: &Value) -> NR {
    let tyv = i.get_prop(ev, &pk("type"))?;
    let ty = i.to_str(&tyv)?;
    let bv = i.get_prop(&ev, &pk("bubbles"))?;
    let bubbles = i.to_bool(&bv);

    // path: target → ancestors → document → window
    let mut path: Vec<Value> = vec![this.clone()];
    if let Some((did, nid)) = node_ref(i, this) {
        let anc = with_ds(i, |ds| {
            let mut v = Vec::new();
            let mut p = ds.docs[did as usize].nodes[nid].parent;
            while let Some(a) = p {
                v.push(a);
                p = ds.docs[did as usize].nodes[a].parent;
            }
            v
        });
        for a in anc {
            path.push(wrap_node(i, did, a));
        }
    }
    let g = Value::Obj(i.p.global.clone());
    if !matches!(path.last(), Some(v) if i.same_value(v, &g).unwrap_or(false)) {
        path.push(g);
    }

    // capture phase: window → … → target
    for target in path.iter().rev() {
        if let Value::Obj(o) = target {
            let fns = listener_fns(i, o, &ty, true);
            if !fns.is_empty() {
                i.set_prop(&ev, pk("eventPhase"), Value::Num(1.0))?;
                i.set_prop(&ev, pk("currentTarget"), target.clone())?;
                for h in fns {
                    i.call(&h, target.clone(), vec![ev.clone()])?;
                    if ev_flag(&ev, K_STOPIMM) {
                        break;
                    }
                }
            }
        }
        if ev_flag(&ev, K_STOP) || ev_flag(&ev, K_STOPIMM) {
            i.set_prop(&ev, pk("eventPhase"), Value::Num(0.0))?;
            i.set_prop(&ev, pk("currentTarget"), Value::Null)?;
            let dp = i.get_prop(&ev, &pk("defaultPrevented"))?;
            return Ok(Value::Bool(!i.to_bool(&dp)));
        }
    }
    // bubble phase: target → … → window
    for (idx, target) in path.iter().enumerate() {
        if idx > 0 && !bubbles {
            break;
        }
        if let Value::Obj(o) = target {
            let fns = listener_fns(i, o, &ty, false);
            if !fns.is_empty() {
                let phase = if idx == 0 { 2.0 } else { 3.0 };
                i.set_prop(&ev, pk("eventPhase"), Value::Num(phase))?;
                i.set_prop(&ev, pk("currentTarget"), target.clone())?;
                for h in fns {
                    i.call(&h, target.clone(), vec![ev.clone()])?;
                    if ev_flag(&ev, K_STOPIMM) {
                        break;
                    }
                }
            }
        }
        if ev_flag(&ev, K_STOP) || ev_flag(&ev, K_STOPIMM) {
            break;
        }
    }
    i.set_prop(&ev, pk("eventPhase"), Value::Num(0.0))?;
    i.set_prop(&ev, pk("currentTarget"), Value::Null)?;
    let dp = i.get_prop(&ev, &pk("defaultPrevented"))?;
    Ok(Value::Bool(!i.to_bool(&dp)))
}

/// hidden stop flags on event objects
fn ev_flag(ev: &Value, key: &str) -> bool {
    let Value::Obj(o) = ev else { return false };
    matches!(
        o.borrow().props.get(&pk(key)).map(|p| p.val.clone()),
        Some(Value::Bool(true))
    )
}

// ---------------------------------------------------------------------------
// document natives
// ---------------------------------------------------------------------------

/// descendants of `from` that are elements satisfying `pred`
fn collect_elems(
    ds: &DomState,
    did: u32,
    from: NodeId,
    pred: impl Fn(&Document, NodeId) -> bool,
) -> Vec<NodeId> {
    let doc = &ds.docs[did as usize];
    doc.descendants(from)
        .filter(|&c| doc.tag(c).is_some() && pred(doc, c))
        .collect()
}

fn wrap_list(i: &mut Interp, did: u32, ids: Vec<NodeId>) -> Value {
    let vals: Vec<Value> = ids.into_iter().map(|n| wrap_node(i, did, n)).collect();
    Value::Obj(i.new_arr(vals))
}

fn doc_get_element_by_id(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, _)) = node_ref(i, this) else {
        return Ok(Value::Null);
    };
    let want = i.to_str(&arg(args, 0))?;
    let found = with_ds(i, |ds| {
        let root = ds.docs[did as usize].root;
        collect_elems(ds, did, root, |d, n| d.attr(n, "id") == Some(want.as_str()))
            .into_iter()
            .next()
    });
    Ok(match found {
        Some(n) => wrap_node(i, did, n),
        None => Value::Null,
    })
}

fn get_by_tag(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Obj(i.new_arr(vec![])));
    };
    let want = i.to_str(&arg(args, 0))?.to_lowercase();
    let ids = with_ds(i, |ds| {
        collect_elems(ds, did, nid, |d, n| {
            want == "*" || d.tag(n) == Some(want.as_str())
        })
    });
    Ok(wrap_list(i, did, ids))
}

fn get_by_class(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Obj(i.new_arr(vec![])));
    };
    let want = i.to_str(&arg(args, 0))?;
    let ids = with_ds(i, |ds| {
        collect_elems(ds, did, nid, |d, n| {
            d.attr(n, "class").is_some_and(|v| {
                want.split_whitespace()
                    .all(|c| v.split_whitespace().any(|x| x == c))
            })
        })
    });
    Ok(wrap_list(i, did, ids))
}

fn get_by_name(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Obj(i.new_arr(vec![])));
    };
    let want = i.to_str(&arg(args, 0))?;
    let ids = with_ds(i, |ds| {
        collect_elems(ds, did, nid, |d, n| {
            d.attr(n, "name") == Some(want.as_str())
        })
    });
    Ok(wrap_list(i, did, ids))
}

/// `selector(s)` → all selectors in the comma list parsed, or None if any fails.
fn parse_sel_list(s: &str) -> Option<Vec<rowser_css::Selector>> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        out.push(rowser_css::parse_selector(part)?);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn sel_matches(ds: &DomState, did: u32, nid: NodeId, sels: &[rowser_css::Selector]) -> bool {
    let doc = &ds.docs[did as usize];
    sels.iter().any(|s| rowser_css::matches(doc, nid, s))
}

fn query_selector(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Null);
    };
    let s = i.to_str(&arg(args, 0))?;
    let found = with_ds(i, |ds| {
        parse_sel_list(&s).and_then(|sels| {
            collect_elems(ds, did, nid, |d, n| {
                d.tag(n).is_some() && sel_matches(ds, did, n, &sels)
            })
            .into_iter()
            .next()
        })
    });
    Ok(match found {
        Some(n) => wrap_node(i, did, n),
        None => Value::Null,
    })
}

fn query_selector_all(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Obj(i.new_arr(vec![])));
    };
    let s = i.to_str(&arg(args, 0))?;
    let ids = with_ds(i, |ds| {
        parse_sel_list(&s)
            .map(|sels| collect_elems(ds, did, nid, |_d, n| sel_matches(ds, did, n, &sels)))
            .unwrap_or_default()
    });
    Ok(wrap_list(i, did, ids))
}

fn el_matches(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Bool(false));
    };
    let s = i.to_str(&arg(args, 0))?;
    Ok(Value::Bool(with_ds(i, |ds| {
        parse_sel_list(&s).is_some_and(|sels| sel_matches(ds, did, nid, &sels))
    })))
}

fn el_closest(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Null);
    };
    let s = i.to_str(&arg(args, 0))?;
    let found = with_ds(i, |ds| {
        let sels = parse_sel_list(&s)?;
        let mut cur = Some(nid);
        while let Some(n) = cur {
            if ds.docs[did as usize].tag(n).is_some() && sel_matches(ds, did, n, &sels) {
                return Some(n);
            }
            cur = ds.docs[did as usize].nodes[n].parent;
        }
        None
    });
    Ok(match found {
        Some(n) => wrap_node(i, did, n),
        None => Value::Null,
    })
}

fn doc_create_element(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, _)) = node_ref(i, this) else {
        return Err(i.type_err("createElement called on non-document"));
    };
    let tag = i.to_str(&arg(args, 0))?.to_lowercase();
    let nid = with_ds(i, |ds| ds.docs[did as usize].create_element(&tag, vec![]));
    Ok(wrap_node(i, did, nid))
}

fn doc_create_element_ns(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, _)) = node_ref(i, this) else {
        return Err(i.type_err("createElementNS called on non-document"));
    };
    let ns = i.to_str(&arg(args, 0))?;
    let tag = i.to_str(&arg(args, 1))?.to_lowercase();
    let nid = with_ds(i, |ds| {
        let n = ds.docs[did as usize].create_element(&tag, vec![]);
        if ns.contains("svg") {
            ds.svg_nodes.insert((did, n));
        }
        n
    });
    let v = wrap_node(i, did, nid);
    if let Value::Obj(o) = &v {
        i.set_builtin(o, K_NS, Value::Str(Rc::from(ns.as_str())));
    }
    Ok(v)
}

fn doc_create_text_node(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, _)) = node_ref(i, this) else {
        return Err(i.type_err("createTextNode called on non-document"));
    };
    let t = i.to_str(&arg(args, 0))?;
    let nid = with_ds(i, |ds| ds.docs[did as usize].create_text(&t));
    Ok(wrap_node(i, did, nid))
}

fn doc_create_comment(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, _)) = node_ref(i, this) else {
        return Err(i.type_err("createComment called on non-document"));
    };
    let t = i.to_str(&arg(args, 0))?;
    let nid = with_ds(i, |ds| ds.docs[did as usize].create_comment(&t));
    Ok(wrap_node(i, did, nid))
}

fn doc_create_fragment(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let _ = args;
    let Some((did, _)) = node_ref(i, this) else {
        return Err(i.type_err("createDocumentFragment called on non-document"));
    };
    let nid = with_ds(i, |ds| {
        ds.docs[did as usize].create_element(FRAG_TAG, vec![])
    });
    Ok(wrap_node(i, did, nid))
}

fn doc_create_attribute(i: &mut Interp, _t: &Value, args: &[Value]) -> NR {
    let name = i.to_str(&arg(args, 0))?;
    let o = i.new_obj();
    i.set_data(&o, "name", Value::Str(Rc::from(name.as_str())));
    i.set_data(&o, "nodeName", Value::Str(Rc::from(name.as_str())));
    i.set_data(&o, "value", Value::Str(Rc::from("")));
    i.set_data(&o, "nodeValue", Value::Str(Rc::from("")));
    Ok(Value::Obj(o))
}

fn doc_get_head(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, _)) = node_ref(i, this) else {
        return Ok(Value::Null);
    };
    let n = with_ds(i, |ds| {
        let d = &ds.docs[did as usize];
        d.find_deep(d.root, "head")
    });
    Ok(match n {
        Some(n) => wrap_node(i, did, n),
        None => Value::Null,
    })
}

fn doc_get_body(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, _)) = node_ref(i, this) else {
        return Ok(Value::Null);
    };
    let n = with_ds(i, |ds| {
        let d = &ds.docs[did as usize];
        d.find_deep(d.root, "body")
    });
    Ok(match n {
        Some(n) => wrap_node(i, did, n),
        None => Value::Null,
    })
}

fn doc_get_docel(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, _)) = node_ref(i, this) else {
        return Ok(Value::Null);
    };
    let n = with_ds(i, |ds| {
        let d = &ds.docs[did as usize];
        d.nodes[d.root]
            .children
            .iter()
            .find(|&&c| d.tag(c).is_some())
            .copied()
    });
    Ok(match n {
        Some(n) => wrap_node(i, did, n),
        None => Value::Null,
    })
}

fn doc_get_title(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, _)) = node_ref(i, this) else {
        return Ok(Value::Str(Rc::from("")));
    };
    Ok(Value::Str(Rc::from(
        with_ds(i, |ds| {
            let d = &ds.docs[did as usize];
            d.find_deep(d.root, "title")
                .map(|t| d.text_content(t))
                .unwrap_or_default()
        })
        .as_str(),
    )))
}

fn doc_set_title(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, _)) = node_ref(i, this) else {
        return Ok(Value::Und);
    };
    let t = i.to_str(&arg(args, 0))?;
    with_ds(i, |ds| {
        let d = &mut ds.docs[did as usize];
        let title = match d.find_deep(d.root, "title") {
            Some(t) => t,
            None => {
                let t = d.create_element("title", vec![]);
                if let Some(h) = d.find_deep(d.root, "head") {
                    d.append(h, t);
                } else if let Some(h) = d.find_deep(d.root, "html") {
                    d.append(h, t);
                }
                t
            }
        };
        remove_children(d, title);
        let tn = d.create_text(&t);
        d.append(title, tn);
    });
    Ok(Value::Und)
}

fn doc_active_element(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, _)) = node_ref(i, this) else {
        return Ok(Value::Null);
    };
    let n = with_ds(i, |ds| {
        ds.focused
            .filter(|&(d, _)| d == did)
            .map(|(_, n)| n)
            .or_else(|| {
                let d = &ds.docs[did as usize];
                d.find_deep(d.root, "body")
                    .or_else(|| d.find_deep(d.root, "html"))
            })
    });
    Ok(match n {
        Some(n) => wrap_node(i, did, n),
        None => Value::Null,
    })
}

fn collection_by_tags(i: &mut Interp, this: &Value, tags: &[&str]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Obj(i.new_arr(vec![])));
    };
    let ids = with_ds(i, |ds| {
        collect_elems(ds, did, nid, |d, n| {
            d.tag(n).is_some_and(|t| tags.contains(&t))
        })
    });
    Ok(wrap_list(i, did, ids))
}

fn doc_links(i: &mut Interp, this: &Value, a: &[Value]) -> NR {
    let _ = a;
    collection_by_tags(i, this, &["a", "area"])
}
fn doc_forms(i: &mut Interp, this: &Value, a: &[Value]) -> NR {
    let _ = a;
    collection_by_tags(i, this, &["form"])
}
fn doc_images(i: &mut Interp, this: &Value, a: &[Value]) -> NR {
    let _ = a;
    collection_by_tags(i, this, &["img"])
}
fn doc_scripts(i: &mut Interp, this: &Value, a: &[Value]) -> NR {
    let _ = a;
    collection_by_tags(i, this, &["script"])
}

fn doc_has_focus(i: &mut Interp, _t: &Value, _a: &[Value]) -> NR {
    let _ = i;
    Ok(Value::Bool(true))
}

fn doc_create_event(i: &mut Interp, _t: &Value, args: &[Value]) -> NR {
    let _ = args;
    let proto = with_ds(i, |ds| ds.p_event.clone());
    let o = obj(Some(proto));
    Ok(Value::Obj(o))
}

fn doc_create_range(i: &mut Interp, _t: &Value, _a: &[Value]) -> NR {
    let o = i.new_obj();
    for m in [
        "setStart",
        "setEnd",
        "collapse",
        "selectNodeContents",
        "selectNode",
        "setStartBefore",
        "setStartAfter",
        "setEndBefore",
        "setEndAfter",
        "deleteContents",
        "detach",
    ] {
        i.set_builtin(&o, m, i.native(m, noop));
    }
    i.set_data(&o, "collapsed", Value::Bool(true));
    i.set_data(&o, "commonAncestorContainer", Value::Null);
    Ok(Value::Obj(o))
}

fn doc_exec_noop(_i: &mut Interp, _t: &Value, _a: &[Value]) -> NR {
    Ok(Value::Bool(false))
}

fn doc_import_node(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((tdid, _)) = node_ref(i, this) else {
        return Err(i.type_err("importNode on non-document"));
    };
    let Some((sdid, snid)) = node_ref(i, &arg(args, 0)) else {
        return Ok(Value::Null);
    };
    let nid = with_ds(i, |ds| {
        if sdid == tdid {
            clone_subtree_inplace(&mut ds.docs[tdid as usize], snid)
        } else if sdid < tdid {
            let (a, b) = ds.docs.split_at_mut(tdid as usize);
            clone_subtree(&mut b[0], &a[sdid as usize], snid)
        } else {
            let (a, b) = ds.docs.split_at_mut(sdid as usize);
            clone_subtree(&mut a[tdid as usize], &b[0], snid)
        }
    });
    Ok(wrap_node(i, tdid, nid))
}

// ---------------------------------------------------------------------------
// node natives (shared by Document / Element / Text via p_node)
// ---------------------------------------------------------------------------

fn node_append_child(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((pdid, pnid)) = node_ref(i, this) else {
        return Err(i.type_err("appendChild called on non-node"));
    };
    let child = arg(args, 0);
    let Some((cdid, cnid)) = node_ref(i, &child) else {
        return Err(i.type_err("appendChild: argument is not a Node"));
    };
    let (_, cnid2) = adopt(i, pdid, cdid, cnid);
    with_ds(i, |ds| {
        let doc = &mut ds.docs[pdid as usize];
        let pos = doc.nodes[pnid].children.len();
        insert_maybe_frag(doc, pnid, cnid2, pos);
    });
    Ok(child)
}

/// Cross-document adopt: clone subtree into target doc.
fn adopt(i: &mut Interp, tdid: u32, sdid: u32, snid: NodeId) -> (u32, NodeId) {
    if tdid == sdid {
        return (tdid, snid);
    }
    let n = with_ds(i, |ds| {
        if sdid < tdid {
            let (a, b) = ds.docs.split_at_mut(tdid as usize);
            clone_subtree(&mut b[0], &a[sdid as usize], snid)
        } else {
            let (a, b) = ds.docs.split_at_mut(sdid as usize);
            clone_subtree(&mut a[tdid as usize], &b[0], snid)
        }
    });
    (tdid, n)
}

fn node_insert_before(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((pdid, pnid)) = node_ref(i, this) else {
        return Err(i.type_err("insertBefore called on non-node"));
    };
    let child = arg(args, 0);
    let Some((cdid, cnid)) = node_ref(i, &child) else {
        return Err(i.type_err("insertBefore: argument is not a Node"));
    };
    let refv = arg(args, 1);
    let refr = node_ref(i, &refv);
    let (_, cnid2) = adopt(i, pdid, cdid, cnid);
    with_ds(i, |ds| {
        let doc = &mut ds.docs[pdid as usize];
        let pos = match refr {
            Some((rdid, rnid)) if rdid == pdid && doc.nodes[rnid].parent == Some(pnid) => doc.nodes
                [pnid]
                .children
                .iter()
                .position(|&c| c == rnid)
                .unwrap_or_else(|| doc.nodes[pnid].children.len()),
            _ => doc.nodes[pnid].children.len(),
        };
        insert_maybe_frag(doc, pnid, cnid2, pos);
    });
    Ok(child)
}

fn node_remove_child(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((pdid, pnid)) = node_ref(i, this) else {
        return Err(i.type_err("removeChild called on non-node"));
    };
    let child = arg(args, 0);
    let Some((cdid, cnid)) = node_ref(i, &child) else {
        return Err(i.type_err("removeChild: argument is not a Node"));
    };
    let ok = with_ds(i, |ds| {
        let doc = &mut ds.docs[pdid as usize];
        if cdid == pdid && doc.nodes[cnid].parent == Some(pnid) {
            detach(doc, cnid);
            true
        } else {
            false
        }
    });
    if !ok {
        return Err(i.plain_err("Failed to execute 'removeChild': node is not a child"));
    }
    Ok(child)
}

fn node_replace_child(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((pdid, pnid)) = node_ref(i, this) else {
        return Err(i.type_err("replaceChild called on non-node"));
    };
    let newv = arg(args, 0);
    let oldv = arg(args, 1);
    let Some((ndid, nnid)) = node_ref(i, &newv) else {
        return Err(i.type_err("replaceChild: argument 1 is not a Node"));
    };
    let Some((odid, onid)) = node_ref(i, &oldv) else {
        return Err(i.type_err("replaceChild: argument 2 is not a Node"));
    };
    let (_, nnid2) = adopt(i, pdid, ndid, nnid);
    let ok = with_ds(i, |ds| {
        let doc = &mut ds.docs[pdid as usize];
        if odid == pdid && doc.nodes[onid].parent == Some(pnid) {
            let pos = doc.nodes[pnid]
                .children
                .iter()
                .position(|&c| c == onid)
                .unwrap();
            detach(doc, onid);
            insert_maybe_frag(doc, pnid, nnid2, pos);
            true
        } else {
            false
        }
    });
    if !ok {
        return Err(i.plain_err("Failed to execute 'replaceChild': node is not a child"));
    }
    Ok(oldv)
}

fn node_clone(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Err(i.type_err("cloneNode called on non-node"));
    };
    let deep = i.to_bool(&arg(args, 0));
    let n = with_ds(i, |ds| {
        if deep {
            clone_subtree_inplace(&mut ds.docs[did as usize], nid)
        } else {
            // shallow: element attrs only / text copy
            let d = &mut ds.docs[did as usize];
            let kind = d.nodes[nid].kind.clone();
            match kind {
                NodeKind::Element { tag, attrs } => {
                    d.create_element(&tag, attrs.into_iter().collect())
                }
                NodeKind::Text(t) => d.create_text(&t),
                NodeKind::Comment(c) => d.create_comment(&c),
                NodeKind::Document => d.create_element(FRAG_TAG, vec![]),
            }
        }
    });
    Ok(wrap_node(i, did, n))
}

fn node_contains(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((adid, anid)) = node_ref(i, this) else {
        return Ok(Value::Bool(false));
    };
    let Some((bdid, bnid)) = node_ref(i, &arg(args, 0)) else {
        return Ok(Value::Bool(false));
    };
    Ok(Value::Bool(with_ds(i, |ds| {
        if adid != bdid {
            return false;
        }
        if anid == bnid {
            return true;
        }
        let mut cur = ds.docs[adid as usize].nodes[bnid].parent;
        while let Some(p) = cur {
            if p == anid {
                return true;
            }
            cur = ds.docs[adid as usize].nodes[p].parent;
        }
        false
    })))
}

fn node_has_children(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Bool(false));
    };
    Ok(Value::Bool(with_ds(i, |ds| {
        !ds.docs[did as usize].nodes[nid].children.is_empty()
    })))
}

fn node_remove(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    if let Some((did, nid)) = node_ref(i, this) {
        with_ds(i, |ds| detach(&mut ds.docs[did as usize], nid));
    }
    Ok(Value::Und)
}

fn node_get_root(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, _)) = node_ref(i, this) else {
        return Ok(Value::Null);
    };
    Ok(doc_obj(i, did))
}

/// node insertion helpers for append/prepend/before/after/replaceWith
fn js_to_node(i: &mut Interp, did: u32, v: &Value) -> Result<Option<NodeId>, Value> {
    if let Some((sdid, snid)) = node_ref(i, v) {
        let (_, n) = adopt(i, did, sdid, snid);
        return Ok(Some(n));
    }
    match v {
        Value::Und | Value::Null => Ok(None),
        _ => {
            let s = i.to_str(v)?;
            Ok(Some(with_ds(i, |ds| ds.docs[did as usize].create_text(&s))))
        }
    }
}

fn node_append(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Und);
    };
    let mut news = Vec::new();
    for a in args {
        if let Some(n) = js_to_node(i, did, a)? {
            news.push(n);
        }
    }
    with_ds(i, |ds| {
        let doc = &mut ds.docs[did as usize];
        for n in news {
            let pos = doc.nodes[nid].children.len();
            insert_maybe_frag(doc, nid, n, pos);
        }
    });
    Ok(Value::Und)
}

fn node_prepend(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Und);
    };
    let mut news = Vec::new();
    for a in args {
        if let Some(n) = js_to_node(i, did, a)? {
            news.push(n);
        }
    }
    with_ds(i, |ds| {
        let doc = &mut ds.docs[did as usize];
        for (k, n) in news.into_iter().enumerate() {
            insert_maybe_frag(doc, nid, n, k);
        }
    });
    Ok(Value::Und)
}

fn node_before(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Und);
    };
    let mut news = Vec::new();
    for a in args {
        if let Some(n) = js_to_node(i, did, a)? {
            news.push(n);
        }
    }
    with_ds(i, |ds| {
        let doc = &mut ds.docs[did as usize];
        if let Some((p, pos)) = pos_in_parent(doc, nid) {
            for (k, n) in news.into_iter().enumerate() {
                insert_maybe_frag(doc, p, n, pos + k);
            }
        }
    });
    Ok(Value::Und)
}

fn node_after(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Und);
    };
    let mut news = Vec::new();
    for a in args {
        if let Some(n) = js_to_node(i, did, a)? {
            news.push(n);
        }
    }
    with_ds(i, |ds| {
        let doc = &mut ds.docs[did as usize];
        if let Some((p, pos)) = pos_in_parent(doc, nid) {
            for (k, n) in news.into_iter().enumerate() {
                insert_maybe_frag(doc, p, n, pos + 1 + k);
            }
        }
    });
    Ok(Value::Und)
}

fn node_replace_with(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Und);
    };
    let mut news = Vec::new();
    for a in args {
        if let Some(n) = js_to_node(i, did, a)? {
            news.push(n);
        }
    }
    with_ds(i, |ds| {
        let doc = &mut ds.docs[did as usize];
        if let Some((p, pos)) = pos_in_parent(doc, nid) {
            detach(doc, nid);
            for (k, n) in news.into_iter().enumerate() {
                insert_maybe_frag(doc, p, n, pos + k);
            }
        }
    });
    Ok(Value::Und)
}

// -- node accessors ---------------------------------------------------------

fn node_child_nodes(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Obj(i.new_arr(vec![])));
    };
    let ids = with_ds(i, |ds| ds.docs[did as usize].nodes[nid].children.clone());
    Ok(wrap_list(i, did, ids))
}

fn node_children(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Obj(i.new_arr(vec![])));
    };
    let ids = with_ds(i, |ds| {
        let d = &ds.docs[did as usize];
        d.nodes[nid]
            .children
            .iter()
            .copied()
            .filter(|&c| d.tag(c).is_some())
            .collect::<Vec<_>>()
    });
    Ok(wrap_list(i, did, ids))
}

fn rel_node(i: &mut Interp, this: &Value, pick: usize) -> NR {
    // pick: 0=firstChild 1=lastChild 2=nextSibling 3=prevSibling
    //       4=parentNode 5=parentElement 6..9 element-only variants
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Null);
    };
    let found = with_ds(i, |ds| {
        let d = &ds.docs[did as usize];
        match pick {
            0 | 6 => d.nodes[nid]
                .children
                .first()
                .copied()
                .filter(|&c| pick == 0 || d.tag(c).is_some()),
            1 | 7 => d.nodes[nid]
                .children
                .last()
                .copied()
                .filter(|&c| pick == 1 || d.tag(c).is_some()),
            2 | 3 | 8 | 9 => {
                let (p, pos) = pos_in_parent(d, nid)?;
                let kids = &d.nodes[p].children;
                match pick {
                    2 => kids.get(pos + 1).copied(),
                    3 => pos.checked_sub(1).and_then(|x| kids.get(x)).copied(),
                    8 => kids[pos + 1..]
                        .iter()
                        .copied()
                        .find(|&c| d.tag(c).is_some()),
                    _ => kids[..pos]
                        .iter()
                        .rev()
                        .copied()
                        .find(|&c| d.tag(c).is_some()),
                }
            }
            4 | 5 => d.nodes[nid]
                .parent
                .filter(|&p| pick == 4 || d.tag(p).is_some()),
            _ => None,
        }
    });
    Ok(match found {
        Some(n) => wrap_node(i, did, n),
        None => Value::Null,
    })
}

fn node_first_child(i: &mut Interp, t: &Value, a: &[Value]) -> NR {
    let _ = a;
    rel_node(i, t, 0)
}
fn node_last_child(i: &mut Interp, t: &Value, a: &[Value]) -> NR {
    let _ = a;
    rel_node(i, t, 1)
}
fn node_next_sibling(i: &mut Interp, t: &Value, a: &[Value]) -> NR {
    let _ = a;
    rel_node(i, t, 2)
}
fn node_prev_sibling(i: &mut Interp, t: &Value, a: &[Value]) -> NR {
    let _ = a;
    rel_node(i, t, 3)
}
fn node_parent(i: &mut Interp, t: &Value, a: &[Value]) -> NR {
    let _ = a;
    rel_node(i, t, 4)
}
fn node_parent_elem(i: &mut Interp, t: &Value, a: &[Value]) -> NR {
    let _ = a;
    rel_node(i, t, 5)
}
fn node_first_elem(i: &mut Interp, t: &Value, a: &[Value]) -> NR {
    let _ = a;
    rel_node(i, t, 6)
}
fn node_last_elem(i: &mut Interp, t: &Value, a: &[Value]) -> NR {
    let _ = a;
    rel_node(i, t, 7)
}
fn node_next_elem(i: &mut Interp, t: &Value, a: &[Value]) -> NR {
    let _ = a;
    rel_node(i, t, 8)
}
fn node_prev_elem(i: &mut Interp, t: &Value, a: &[Value]) -> NR {
    let _ = a;
    rel_node(i, t, 9)
}

fn node_node_type(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Num(0.0));
    };
    Ok(Value::Num(with_ds(i, |ds| {
        match ds.docs[did as usize].kind(nid) {
            NodeKind::Document => 9.0,
            NodeKind::Element { tag, .. } => {
                if tag == FRAG_TAG {
                    11.0
                } else {
                    1.0
                }
            }
            NodeKind::Text(_) => 3.0,
            NodeKind::Comment(_) => 8.0,
        }
    })))
}

fn node_node_name(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Und);
    };
    let s = with_ds(i, |ds| match ds.docs[did as usize].kind(nid) {
        NodeKind::Document => "#document".to_string(),
        NodeKind::Element { tag, .. } => {
            if tag == FRAG_TAG {
                "#document-fragment".into()
            } else {
                tag.to_uppercase()
            }
        }
        NodeKind::Text(_) => "#text".into(),
        NodeKind::Comment(_) => "#comment".into(),
    });
    Ok(Value::Str(Rc::from(s.as_str())))
}

fn node_tag_name(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Und);
    };
    Ok(with_ds(i, |ds| match ds.docs[did as usize].tag(nid) {
        Some(t) if t != FRAG_TAG => Value::Str(Rc::from(t.to_uppercase().as_str())),
        _ => Value::Und,
    }))
}

fn node_node_value_get(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Null);
    };
    Ok(with_ds(i, |ds| match ds.docs[did as usize].kind(nid) {
        NodeKind::Text(t) | NodeKind::Comment(t) => Value::Str(Rc::from(t.as_str())),
        _ => Value::Null,
    }))
}

fn node_node_value_set(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let v = i.to_str(&arg(args, 0))?;
    if let Some((did, nid)) = node_ref(i, this) {
        with_ds(i, |ds| {
            let k = &mut ds.docs[did as usize].nodes[nid].kind;
            match k {
                NodeKind::Text(t) | NodeKind::Comment(t) => *t = v,
                _ => {}
            }
        });
    }
    Ok(Value::Und)
}

fn node_text_content_get(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Null);
    };
    Ok(with_ds(i, |ds| match ds.docs[did as usize].kind(nid) {
        NodeKind::Document => Value::Null,
        NodeKind::Text(t) | NodeKind::Comment(t) => Value::Str(Rc::from(t.as_str())),
        _ => Value::Str(Rc::from(ds.docs[did as usize].text_content(nid).as_str())),
    }))
}

fn node_text_content_set(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let v = i.to_str(&arg(args, 0))?;
    if let Some((did, nid)) = node_ref(i, this) {
        with_ds(i, |ds| {
            let doc = &mut ds.docs[did as usize];
            match &doc.nodes[nid].kind {
                NodeKind::Text(_) | NodeKind::Comment(_) => {
                    if let NodeKind::Text(t) | NodeKind::Comment(t) = &mut doc.nodes[nid].kind {
                        *t = v;
                    }
                }
                NodeKind::Document => {}
                NodeKind::Element { .. } => {
                    remove_children(doc, nid);
                    if !v.is_empty() {
                        let tn = doc.create_text(&v);
                        doc.append(nid, tn);
                    }
                }
            }
        });
    }
    Ok(Value::Und)
}

fn node_owner_document(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, _)) = node_ref(i, this) else {
        return Ok(Value::Null);
    };
    Ok(doc_obj(i, did))
}

fn node_is_connected(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Bool(false));
    };
    Ok(Value::Bool(with_ds(i, |ds| {
        let d = &ds.docs[did as usize];
        let mut cur = Some(nid);
        while let Some(n) = cur {
            if matches!(d.nodes[n].kind, NodeKind::Document) {
                return true;
            }
            cur = d.nodes[n].parent;
        }
        false
    })))
}

fn node_base_uri(i: &mut Interp, _t: &Value, _a: &[Value]) -> NR {
    Ok(Value::Str(Rc::from(
        with_ds(i, |ds| ds.url.clone()).as_str(),
    )))
}

fn el_local_name(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Und);
    };
    Ok(with_ds(i, |ds| {
        ds.docs[did as usize]
            .tag(nid)
            .map(|t| Value::Str(Rc::from(t)))
            .unwrap_or(Value::Und)
    }))
}

fn el_namespace_uri(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Null);
    };
    let ns_prop = match this {
        Value::Obj(o) => i.obj_get_raw(o, &pk(K_NS)),
        _ => None,
    };
    let is_svg = with_ds(i, |ds| {
        ds.svg_nodes.contains(&(did, nid)) || ds.docs[did as usize].tag(nid) == Some("svg")
    }) || matches!(ns_prop, Some(Value::Str(s)) if s.contains("svg"));
    Ok(Value::Str(Rc::from(if is_svg {
        "http://www.w3.org/2000/svg"
    } else {
        "http://www.w3.org/1999/xhtml"
    })))
}

// ---------------------------------------------------------------------------
// element natives
// ---------------------------------------------------------------------------

fn el_get_attribute(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Null);
    };
    let name = i.to_str(&arg(args, 0))?.to_lowercase();
    Ok(with_ds(i, |ds| {
        match ds.docs[did as usize].attr(nid, &name) {
            Some(v) => Value::Str(Rc::from(v)),
            None => Value::Null,
        }
    }))
}

/// `*NS` variants take (ns, name, …) — ignore the namespace arg.
fn rest_args(args: &[Value]) -> &[Value] {
    if args.len() > 1 {
        &args[1..]
    } else {
        &[]
    }
}

fn el_get_attribute_ns(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    el_get_attribute(i, this, rest_args(args))
}

fn el_set_attribute(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Und);
    };
    let name = i.to_str(&arg(args, 0))?.to_lowercase();
    let v = i.to_str(&arg(args, 1))?;
    with_ds(i, |ds| attr_set(&mut ds.docs[did as usize], nid, &name, &v));
    Ok(Value::Und)
}

fn el_set_attribute_ns(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    el_set_attribute(i, this, rest_args(args))
}

fn el_remove_attribute(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Und);
    };
    let name = i.to_str(&arg(args, 0))?.to_lowercase();
    with_ds(i, |ds| attr_remove(&mut ds.docs[did as usize], nid, &name));
    Ok(Value::Und)
}

fn el_remove_attribute_ns(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    el_remove_attribute(i, this, rest_args(args))
}

fn el_has_attribute(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Bool(false));
    };
    let name = i.to_str(&arg(args, 0))?.to_lowercase();
    Ok(Value::Bool(with_ds(i, |ds| {
        ds.docs[did as usize].attr(nid, &name).is_some()
    })))
}

fn el_has_attribute_ns(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    el_has_attribute(i, this, rest_args(args))
}

fn el_toggle_attribute(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Bool(false));
    };
    let name = i.to_str(&arg(args, 0))?.to_lowercase();
    let force = arg(args, 1);
    let has = with_ds(i, |ds| ds.docs[did as usize].attr(nid, &name).is_some());
    let want = if matches!(force, Value::Und) {
        !has
    } else {
        i.to_bool(&force)
    };
    with_ds(i, |ds| {
        if want {
            attr_set(&mut ds.docs[did as usize], nid, &name, "");
        } else {
            attr_remove(&mut ds.docs[did as usize], nid, &name);
        }
    });
    Ok(Value::Bool(want))
}

fn el_attribute_names(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Obj(i.new_arr(vec![])));
    };
    let names = with_ds(i, |ds| match ds.docs[did as usize].kind(nid) {
        NodeKind::Element { attrs, .. } => attrs.keys().cloned().collect::<Vec<_>>(),
        _ => vec![],
    });
    Ok(Value::Obj(
        i.new_arr(
            names
                .into_iter()
                .map(|n| Value::Str(Rc::from(n.as_str())))
                .collect(),
        ),
    ))
}

fn el_attributes(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Obj(i.new_arr(vec![])));
    };
    let attrs = with_ds(i, |ds| match ds.docs[did as usize].kind(nid) {
        NodeKind::Element { attrs, .. } => attrs
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<Vec<_>>(),
        _ => vec![],
    });
    let items: Vec<Value> = attrs
        .into_iter()
        .map(|(k, v)| {
            let o = i.new_obj();
            i.set_data(&o, "name", Value::Str(Rc::from(k.as_str())));
            i.set_data(&o, "nodeName", Value::Str(Rc::from(k.as_str())));
            i.set_data(&o, "value", Value::Str(Rc::from(v.as_str())));
            i.set_data(&o, "nodeValue", Value::Str(Rc::from(v.as_str())));
            i.set_data(&o, "specified", Value::Bool(true));
            Value::Obj(o)
        })
        .collect();
    let arr = i.new_arr(items);
    i.set_builtin(&arr, "item", i.native("item", list_item));
    Ok(Value::Obj(arr))
}

fn list_item(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let n = i.to_num(&arg(args, 0))? as usize;
    if let Value::Obj(o) = this {
        if let ObjKind::Arr(items) = &o.borrow().kind {
            return Ok(items.get(n).cloned().unwrap_or(Value::Null));
        }
    }
    Ok(Value::Null)
}

fn el_id_get(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Str(Rc::from("")));
    };
    Ok(Value::Str(Rc::from(
        with_ds(i, |ds| {
            ds.docs[did as usize]
                .attr(nid, "id")
                .unwrap_or("")
                .to_string()
        })
        .as_str(),
    )))
}

fn el_id_set(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let v = i.to_str(&arg(args, 0))?;
    if let Some((did, nid)) = node_ref(i, this) {
        with_ds(i, |ds| attr_set(&mut ds.docs[did as usize], nid, "id", &v));
    }
    Ok(Value::Und)
}

fn el_classname_get(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Str(Rc::from("")));
    };
    Ok(Value::Str(Rc::from(
        with_ds(i, |ds| {
            ds.docs[did as usize]
                .attr(nid, "class")
                .unwrap_or("")
                .to_string()
        })
        .as_str(),
    )))
}

fn el_classname_set(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let v = i.to_str(&arg(args, 0))?;
    if let Some((did, nid)) = node_ref(i, this) {
        with_ds(i, |ds| {
            attr_set(&mut ds.docs[did as usize], nid, "class", &v)
        });
    }
    Ok(Value::Und)
}

/// reflected string attr get/set — macro-generated natives
macro_rules! reflect_str {
    ($get:ident, $set:ident, $attr:literal) => {
        fn $get(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
            let Some((did, nid)) = node_ref(i, this) else {
                return Ok(Value::Str(Rc::from("")));
            };
            Ok(Value::Str(Rc::from(
                with_ds(i, |ds| {
                    ds.docs[did as usize]
                        .attr(nid, $attr)
                        .unwrap_or("")
                        .to_string()
                })
                .as_str(),
            )))
        }
        fn $set(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
            let v = i.to_str(&arg(args, 0))?;
            if let Some((did, nid)) = node_ref(i, this) {
                with_ds(i, |ds| attr_set(&mut ds.docs[did as usize], nid, $attr, &v));
            }
            Ok(Value::Und)
        }
    };
}

/// reflected boolean attr: present ⇄ true
macro_rules! reflect_bool {
    ($get:ident, $set:ident, $attr:literal) => {
        fn $get(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
            let Some((did, nid)) = node_ref(i, this) else {
                return Ok(Value::Bool(false));
            };
            Ok(Value::Bool(with_ds(i, |ds| {
                ds.docs[did as usize].attr(nid, $attr).is_some()
            })))
        }
        fn $set(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
            let v = i.to_bool(&arg(args, 0));
            if let Some((did, nid)) = node_ref(i, this) {
                with_ds(i, |ds| {
                    if v {
                        attr_set(&mut ds.docs[did as usize], nid, $attr, "");
                    } else {
                        attr_remove(&mut ds.docs[did as usize], nid, $attr);
                    }
                });
            }
            Ok(Value::Und)
        }
    };
}

reflect_str!(val_get, val_set, "value");
reflect_str!(href_get, href_set, "href");
reflect_str!(src_get, src_set, "src");
reflect_str!(alt_get, alt_set, "alt");
reflect_str!(name_get, name_set, "name");
reflect_str!(type_get, type_set, "type");
reflect_str!(ph_get, ph_set, "placeholder");
reflect_str!(rel_get, rel_set, "rel");
reflect_str!(lang_get, lang_set, "lang");
reflect_str!(dir_get, dir_set, "dir");
reflect_str!(for_get, for_set, "for");
reflect_str!(action_get, action_set, "action");
reflect_str!(method_get, method_set, "method");
reflect_str!(title_attr_get, title_attr_set, "title");
reflect_bool!(checked_get, checked_set, "checked");
reflect_bool!(disabled_get, disabled_set, "disabled");
reflect_bool!(selected_get, selected_set, "selected");
reflect_bool!(readonly_get, readonly_set, "readonly");
reflect_bool!(multiple_get, multiple_set, "multiple");
reflect_bool!(required_get, required_set, "required");

fn tab_index_get(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Num(-1.0));
    };
    Ok(Value::Num(with_ds(i, |ds| {
        ds.docs[did as usize]
            .attr(nid, "tabindex")
            .and_then(|s| s.trim().parse::<f64>().ok())
            .unwrap_or(-1.0)
    })))
}

fn tab_index_set(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let v = i.to_num(&arg(args, 0))?;
    if let Some((did, nid)) = node_ref(i, this) {
        with_ds(i, |ds| {
            attr_set(&mut ds.docs[did as usize], nid, "tabindex", &js_num_str(v))
        });
    }
    Ok(Value::Und)
}

fn el_inner_html_get(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Str(Rc::from("")));
    };
    let s = with_ds(i, |ds| {
        let mut out = String::new();
        serialize_children(&ds.docs[did as usize], nid, &mut out);
        out
    });
    Ok(Value::Str(Rc::from(s.as_str())))
}

fn el_inner_html_set(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let v = i.to_str(&arg(args, 0))?;
    if let Some((did, nid)) = node_ref(i, this) {
        with_ds(i, |ds| set_inner_html(ds, did, nid, &v));
    }
    Ok(Value::Und)
}

fn el_outer_html_get(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Str(Rc::from("")));
    };
    let s = with_ds(i, |ds| {
        let mut out = String::new();
        serialize(&ds.docs[did as usize], nid, &mut out);
        out
    });
    Ok(Value::Str(Rc::from(s.as_str())))
}

fn el_outer_html_set(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let v = i.to_str(&arg(args, 0))?;
    if let Some((did, nid)) = node_ref(i, this) {
        with_ds(i, |ds| {
            // parse replacement into a fragment, then swap it in
            let parsed = rowser_html::parse(&v);
            let src = parsed.find_deep(parsed.root, "body").unwrap_or(parsed.root);
            let frag = ds.docs[did as usize].create_element(FRAG_TAG, vec![]);
            graft_children(&mut ds.docs[did as usize], &parsed, src, frag);
            let doc = &mut ds.docs[did as usize];
            if let Some((p, pos)) = pos_in_parent(doc, nid) {
                detach(doc, nid);
                insert_maybe_frag(doc, p, frag, pos);
            }
        });
    }
    Ok(Value::Und)
}

fn el_insert_adjacent_html(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Und);
    };
    let pos = i.to_str(&arg(args, 0))?.to_lowercase();
    let html = i.to_str(&arg(args, 1))?;
    with_ds(i, |ds| {
        let parsed = rowser_html::parse(&html);
        let src = parsed.find_deep(parsed.root, "body").unwrap_or(parsed.root);
        let frag = ds.docs[did as usize].create_element(FRAG_TAG, vec![]);
        graft_children(&mut ds.docs[did as usize], &parsed, src, frag);
        let doc = &mut ds.docs[did as usize];
        match pos.as_str() {
            "beforebegin" => {
                if let Some((p, at)) = pos_in_parent(doc, nid) {
                    insert_maybe_frag(doc, p, frag, at);
                }
            }
            "afterbegin" => insert_maybe_frag(doc, nid, frag, 0),
            "beforeend" => {
                let at = doc.nodes[nid].children.len();
                insert_maybe_frag(doc, nid, frag, at);
            }
            "afterend" => {
                if let Some((p, at)) = pos_in_parent(doc, nid) {
                    insert_maybe_frag(doc, p, frag, at + 1);
                }
            }
            _ => {}
        }
    });
    Ok(Value::Und)
}

fn el_focus(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    if let Some((did, nid)) = node_ref(i, this) {
        with_ds(i, |ds| ds.focused = Some((did, nid)));
    }
    Ok(Value::Und)
}

fn el_blur(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    if let Some((did, nid)) = node_ref(i, this) {
        with_ds(i, |ds| {
            if ds.focused == Some((did, nid)) {
                ds.focused = None;
            }
        });
    }
    Ok(Value::Und)
}

fn el_click(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let ev = make_event(i, "click", true, true);
    el_dispatch_event(i, this, &[ev])
}

fn el_get_rect(i: &mut Interp, _t: &Value, _a: &[Value]) -> NR {
    let o = i.new_obj();
    for k in [
        "x", "y", "top", "left", "right", "bottom", "width", "height",
    ] {
        i.set_data(&o, k, Value::Num(0.0));
    }
    Ok(Value::Obj(o))
}

fn el_get_rects(i: &mut Interp, t: &Value, a: &[Value]) -> NR {
    let r = el_get_rect(i, t, a)?;
    Ok(Value::Obj(i.new_arr(vec![r])))
}

fn zero(i: &mut Interp, _t: &Value, _a: &[Value]) -> NR {
    let _ = i;
    Ok(Value::Num(0.0))
}

fn noop(_i: &mut Interp, _t: &Value, _a: &[Value]) -> NR {
    Ok(Value::Und)
}

// -- classList ----------------------------------------------------------------

fn classlist_obj(i: &mut Interp, el: &Value, el_o: &ObjRef) -> Value {
    if let Some(v) = i.obj_get_raw(el_o, &pk(K_CLASSLIST)) {
        return v;
    }
    let proto = with_ds(i, |ds| ds.p_classlist.clone());
    let o = obj(Some(proto));
    i.set_builtin(&o, K_EL, el.clone());
    i.set_builtin(el_o, K_CLASSLIST, Value::Obj(o.clone()));
    Value::Obj(o)
}

fn cl_tokens(i: &mut Interp, this: &Value) -> Option<(u32, NodeId, Vec<String>)> {
    let Value::Obj(o) = this else { return None };
    let el = i.obj_get_raw(o, &pk(K_EL))?;
    let (did, nid) = node_ref(i, &el)?;
    let cur = with_ds(i, |ds| {
        ds.docs[did as usize]
            .attr(nid, "class")
            .unwrap_or("")
            .to_string()
    });
    Some((
        did,
        nid,
        cur.split_whitespace().map(|s| s.to_string()).collect(),
    ))
}

fn cl_set(i: &mut Interp, did: u32, nid: NodeId, toks: &[String]) {
    with_ds(i, |ds| {
        attr_set(&mut ds.docs[did as usize], nid, "class", &toks.join(" "))
    });
}

fn cl_add(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, nid, mut toks)) = cl_tokens(i, this) else {
        return Ok(Value::Und);
    };
    for a in args {
        let s = i.to_str(a)?;
        if !s.is_empty() && !toks.iter().any(|t| *t == s) {
            toks.push(s);
        }
    }
    cl_set(i, did, nid, &toks);
    Ok(Value::Und)
}

fn cl_remove(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, nid, mut toks)) = cl_tokens(i, this) else {
        return Ok(Value::Und);
    };
    for a in args {
        let s = i.to_str(a)?;
        toks.retain(|t| *t != s);
    }
    cl_set(i, did, nid, &toks);
    Ok(Value::Und)
}

fn cl_toggle(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, nid, mut toks)) = cl_tokens(i, this) else {
        return Ok(Value::Bool(false));
    };
    let s = i.to_str(&arg(args, 0))?;
    let has = toks.iter().any(|t| *t == s);
    let force = arg(args, 1);
    let want = if matches!(force, Value::Und) {
        !has
    } else {
        i.to_bool(&force)
    };
    match (has, want) {
        (false, true) => toks.push(s),
        (true, false) => toks.retain(|t| *t != s),
        _ => {}
    }
    cl_set(i, did, nid, &toks);
    Ok(Value::Bool(want))
}

fn cl_contains(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((_, _, toks)) = cl_tokens(i, this) else {
        return Ok(Value::Bool(false));
    };
    let s = i.to_str(&arg(args, 0))?;
    Ok(Value::Bool(toks.iter().any(|t| *t == s)))
}

fn cl_to_string(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((_, _, toks)) = cl_tokens(i, this) else {
        return Ok(Value::Str(Rc::from("")));
    };
    Ok(Value::Str(Rc::from(toks.join(" ").as_str())))
}

fn cl_length(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((_, _, toks)) = cl_tokens(i, this) else {
        return Ok(Value::Num(0.0));
    };
    Ok(Value::Num(toks.len() as f64))
}

// -- style --------------------------------------------------------------------

/// camelCase → kebab-case (`cssFloat` → `float`, `WebkitX` → `-webkit-x`)
fn kebab(s: &str) -> String {
    if s == "cssFloat" {
        return "float".into();
    }
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            out.push('-');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

const STYLE_PROPS: &[&str] = &[
    "cssText",
    "display",
    "position",
    "top",
    "left",
    "right",
    "bottom",
    "width",
    "height",
    "minWidth",
    "minHeight",
    "maxWidth",
    "maxHeight",
    "margin",
    "marginTop",
    "marginRight",
    "marginBottom",
    "marginLeft",
    "padding",
    "paddingTop",
    "paddingRight",
    "paddingBottom",
    "paddingLeft",
    "color",
    "background",
    "backgroundColor",
    "backgroundImage",
    "border",
    "borderRadius",
    "borderColor",
    "borderWidth",
    "borderStyle",
    "borderTop",
    "borderBottom",
    "font",
    "fontSize",
    "fontFamily",
    "fontWeight",
    "fontStyle",
    "lineHeight",
    "textAlign",
    "textDecoration",
    "textTransform",
    "verticalAlign",
    "visibility",
    "opacity",
    "overflow",
    "overflowX",
    "overflowY",
    "zIndex",
    "transform",
    "transformOrigin",
    "transition",
    "animation",
    "animationName",
    "animationDuration",
    "boxShadow",
    "boxSizing",
    "content",
    "cursor",
    "flex",
    "flexDirection",
    "flexWrap",
    "flexGrow",
    "flexShrink",
    "flexBasis",
    "justifyContent",
    "alignItems",
    "alignContent",
    "alignSelf",
    "gap",
    "order",
    "gridTemplateColumns",
    "gridTemplateRows",
    "whiteSpace",
    "wordBreak",
    "pointerEvents",
    "userSelect",
    "float",
    "clear",
    "listStyle",
    "listStyleType",
    "objectFit",
    "outline",
    "clipPath",
    "filter",
    "willChange",
    "letterSpacing",
    "wordSpacing",
    "scrollBehavior",
    "aspectRatio",
];

/// fresh CSS-style declaration object for `el` (`el` must be a wrapped node).
fn style_obj(i: &mut Interp, el: &Value, proto: ObjRef) -> ObjRef {
    let o = obj(Some(proto));
    i.set_builtin(&o, K_EL, el.clone());
    i.set_builtin(&o, K_DECLS, Value::Obj(i.new_obj()));
    for k in STYLE_PROPS {
        o.borrow_mut()
            .props
            .insert(pk(k), Prop::data(Value::Str(Rc::from(""))));
    }
    o
}

fn style_get(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Value::Obj(el_o) = this else {
        return Ok(Value::Und);
    };
    if let Some(v) = i.obj_get_raw(el_o, &pk(K_STYLE)) {
        return Ok(v);
    }
    let proto = with_ds(i, |ds| ds.p_style.clone());
    let o = style_obj(i, this, proto);
    i.set_builtin(el_o, K_STYLE, Value::Obj(o.clone()));
    Ok(Value::Obj(o))
}

/// the `__decls` obj of a style object
fn decls_of(i: &Interp, style: &ObjRef) -> Option<ObjRef> {
    match i.obj_get_raw(style, &pk(K_DECLS)) {
        Some(Value::Obj(d)) => Some(d),
        _ => None,
    }
}

/// merge `__decls` + direct camelCase props + `cssText` → `style` attr
fn sync_style_attr(i: &Interp, style: &ObjRef) -> Option<(u32, NodeId, String)> {
    let el = i.obj_get_raw(style, &pk(K_EL))?;
    let (did, nid) = node_ref(i, &el)?;
    let b = style.borrow();
    let mut parts: Vec<(String, String)> = Vec::new();
    let mut raw_css: Option<String> = None;
    for (k, p) in b.props.iter() {
        let PropKey::S(ks) = k else { continue };
        if ks.starts_with('\u{0}') {
            continue;
        }
        if ks.as_ref() == "cssText" {
            // only an explicitly-written non-empty cssText overrides
            if let Value::Str(s) = &p.val {
                if !s.is_empty() {
                    raw_css = Some(s.to_string());
                }
            }
            continue;
        }
        if let Value::Str(s) = &p.val {
            if !s.is_empty() {
                parts.push((kebab(ks), s.to_string()));
            }
        }
    }
    drop(b);
    // decls from setProperty win over prefilled props
    if let Some(d) = decls_of(i, style) {
        let db = d.borrow();
        for (k, p) in db.props.iter() {
            if let (PropKey::S(ks), Value::Str(v)) = (k, &p.val) {
                parts.retain(|(pk2, _)| pk2.as_str() != ks.as_ref());
                parts.push((ks.to_string(), v.to_string()));
            }
        }
    }
    if let Some(css) = raw_css {
        return Some((did, nid, css));
    }
    if parts.is_empty() {
        return Some((did, nid, String::new()));
    }
    let s = parts
        .iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join("; ");
    Some((did, nid, s))
}

fn style_set_property(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Value::Obj(o) = this else {
        return Ok(Value::Und);
    };
    let k = i.to_str(&arg(args, 0))?;
    let v = i.to_str(&arg(args, 1))?;
    if let Some(d) = decls_of(i, o) {
        i.set_data(&d, &k, Value::Str(Rc::from(v.as_str())));
    }
    // reflect immediately to the attr
    if let Some((did, nid, css)) = sync_style_attr(i, o) {
        with_ds(i, |ds| {
            attr_set(&mut ds.docs[did as usize], nid, "style", &css)
        });
    }
    Ok(Value::Und)
}

fn style_get_property(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Value::Obj(o) = this else {
        return Ok(Value::Str(Rc::from("")));
    };
    let k = i.to_str(&arg(args, 0))?;
    // decls first
    if let Some(d) = decls_of(i, o) {
        if let Some(Value::Str(v)) = i.obj_get_raw(&d, &pk(&k)) {
            return Ok(Value::Str(v));
        }
    }
    // own prop (camelCase writes land here)
    if let Some(v) = i.obj_get_raw(o, &pk(&k)) {
        if matches!(v, Value::Str(_) | Value::Num(_)) {
            return Ok(v);
        }
    }
    // fall back to parsing the style attr
    if let Some(el) = i.obj_get_raw(o, &pk(K_EL)) {
        if let Some((did, nid)) = node_ref(i, &el) {
            let v = with_ds(i, |ds| {
                ds.docs[did as usize]
                    .attr(nid, "style")
                    .unwrap_or("")
                    .split(';')
                    .filter_map(|d| d.split_once(':'))
                    .find(|(pk, _)| pk.trim() == k)
                    .map(|(_, v)| v.trim().to_string())
            });
            if let Some(v) = v {
                return Ok(Value::Str(Rc::from(v.as_str())));
            }
        }
    }
    Ok(Value::Str(Rc::from("")))
}

fn style_remove_property(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Value::Obj(o) = this else {
        return Ok(Value::Str(Rc::from("")));
    };
    let k = i.to_str(&arg(args, 0))?;
    if let Some(d) = decls_of(i, o) {
        d.borrow_mut().props.remove(&pk(&k));
    }
    if let Some((did, nid, css)) = sync_style_attr(i, o) {
        with_ds(i, |ds| {
            attr_set(&mut ds.docs[did as usize], nid, "style", &css)
        });
    }
    Ok(Value::Str(Rc::from("")))
}

// -- dataset ------------------------------------------------------------------

fn dataset_obj(i: &mut Interp, el: &Value, el_o: &ObjRef) -> Value {
    if let Some(v) = i.obj_get_raw(el_o, &pk(K_DATASET)) {
        return v;
    }
    let o = i.new_obj();
    i.set_builtin(&o, K_EL, el.clone());
    i.set_builtin(el_o, K_DATASET, Value::Obj(o.clone()));
    // populate from data-* attrs
    if let Some((did, nid)) = node_ref(i, el) {
        let attrs = with_ds(i, |ds| match ds.docs[did as usize].kind(nid) {
            NodeKind::Element { attrs, .. } => attrs
                .iter()
                .filter_map(|(k, v)| k.strip_prefix("data-").map(|r| (r.to_string(), v.clone())))
                .collect::<Vec<_>>(),
            _ => vec![],
        });
        for (k, v) in attrs {
            let ck = datakey_to_camel(&k);
            i.set_data(&o, &ck, Value::Str(Rc::from(v.as_str())));
        }
    }
    Value::Obj(o)
}

fn datakey_to_camel(k: &str) -> String {
    let mut out = String::new();
    let mut up = false;
    for c in k.chars() {
        if c == '-' {
            up = true;
        } else if up {
            out.push(c.to_ascii_uppercase());
            up = false;
        } else {
            out.push(c);
        }
    }
    out
}

fn dataset_get(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Value::Obj(el_o) = this else {
        return Ok(Value::Und);
    };
    Ok(dataset_obj(i, this, el_o))
}

fn classlist_get(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Value::Obj(el_o) = this else {
        return Ok(Value::Und);
    };
    Ok(classlist_obj(i, this, el_o))
}

/// push JS-side prop writes (style props, dataset entries) back into attrs;
/// called before the Document is handed back to the pipeline.
fn sync_dom_side_effects(i: &mut Interp) {
    let objs = with_ds(i, |ds| ds.nodes.values().cloned().collect::<Vec<_>>());
    let mut style_writes: Vec<(u32, NodeId, String)> = Vec::new();
    let mut data_writes: Vec<(u32, NodeId, Vec<(String, String)>)> = Vec::new();
    for o in &objs {
        if let Some(Value::Obj(so)) = i.obj_get_raw(o, &pk(K_STYLE)) {
            if let Some(w) = sync_style_attr(i, &so) {
                style_writes.push(w);
            }
        }
        if let Some(Value::Obj(dso)) = i.obj_get_raw(o, &pk(K_DATASET)) {
            if let Some(el) = i.obj_get_raw(&dso, &pk(K_EL)) {
                if let Some((did, nid)) = node_ref(i, &el) {
                    let mut pairs = Vec::new();
                    for (k, p) in dso.borrow().props.iter() {
                        if let (PropKey::S(ks), Value::Str(v)) = (k, &p.val) {
                            if !ks.starts_with('\u{0}') {
                                pairs.push((format!("data-{}", kebab(ks)), v.to_string()));
                            }
                        }
                    }
                    data_writes.push((did, nid, pairs));
                }
            }
        }
    }
    with_ds(i, |ds| {
        for (did, nid, css) in style_writes {
            attr_set(&mut ds.docs[did as usize], nid, "style", &css);
        }
        for (did, nid, pairs) in data_writes {
            for (k, v) in pairs {
                attr_set(&mut ds.docs[did as usize], nid, &k, &v);
            }
        }
    });
}

// ---------------------------------------------------------------------------
// text-node extras
// ---------------------------------------------------------------------------

fn char_data_get(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    node_node_value_get(i, this, _a)
}

fn char_data_set(i: &mut Interp, this: &Value, a: &[Value]) -> NR {
    node_node_value_set(i, this, a)
}

fn char_length(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Num(0.0));
    };
    Ok(Value::Num(with_ds(i, |ds| {
        match ds.docs[did as usize].kind(nid) {
            NodeKind::Text(t) | NodeKind::Comment(t) => t.chars().count() as f64,
            _ => 0.0,
        }
    })))
}

fn text_split(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Some((did, nid)) = node_ref(i, this) else {
        return Ok(Value::Null);
    };
    let off = i.to_num(&arg(args, 0))? as usize;
    let nid2 = with_ds(i, |ds| {
        let doc = &mut ds.docs[did as usize];
        let t = match doc.kind(nid) {
            NodeKind::Text(t) => t.clone(),
            _ => return None,
        };
        let byte_off = t.char_indices().nth(off).map(|(b, _)| b).unwrap_or(t.len());
        let tail = t[byte_off..].to_string();
        if let NodeKind::Text(tt) = &mut doc.nodes[nid].kind {
            *tt = t[..byte_off].to_string();
        }
        let n2 = doc.create_text(&tail);
        if let Some((p, pos)) = pos_in_parent(doc, nid) {
            insert_at(doc, p, n2, pos + 1);
        }
        Some(n2)
    });
    Ok(match nid2 {
        Some(n) => wrap_node(i, did, n),
        None => Value::Null,
    })
}

// ---------------------------------------------------------------------------
// event constructors
// ---------------------------------------------------------------------------

fn make_event(i: &mut Interp, ty: &str, bubbles: bool, cancelable: bool) -> Value {
    let proto = with_ds(i, |ds| ds.p_event.clone());
    let o = obj(Some(proto));
    i.set_builtin(&o, "type", Value::Str(Rc::from(ty)));
    i.set_builtin(&o, "bubbles", Value::Bool(bubbles));
    i.set_builtin(&o, "cancelable", Value::Bool(cancelable));
    i.set_builtin(&o, "composed", Value::Bool(false));
    i.set_builtin(&o, "defaultPrevented", Value::Bool(false));
    i.set_builtin(&o, "isTrusted", Value::Bool(false));
    i.set_builtin(&o, "eventPhase", Value::Num(0.0));
    i.set_builtin(&o, "target", Value::Null);
    i.set_builtin(&o, "srcElement", Value::Null);
    i.set_builtin(&o, "currentTarget", Value::Null);
    i.set_builtin(&o, K_STOP, Value::Bool(false));
    i.set_builtin(&o, K_STOPIMM, Value::Bool(false));
    let ms = with_ds(i, |ds| ds.t0.elapsed().as_secs_f64() * 1000.0);
    i.set_builtin(&o, "timeStamp", Value::Num(ms));
    i.set_builtin(&o, "detail", Value::Null);
    Value::Obj(o)
}

fn event_init_from(i: &mut Interp, this: &Value, args: &[Value], custom: bool) -> NR {
    let ty = i.to_str(&arg(args, 0))?;
    i.set_builtin(
        match this {
            Value::Obj(o) => o,
            _ => return Ok(Value::Und),
        },
        "type",
        Value::Str(Rc::from(ty.as_str())),
    );
    if let Value::Obj(o) = this {
        let init = arg(args, 1);
        for k in ["bubbles", "cancelable", "composed"] {
            let v = i.get_prop(&init, &pk(k))?;
            if !matches!(v, Value::Und) {
                i.set_builtin(o, k, Value::Bool(i.to_bool(&v)));
            }
        }
        if custom {
            let d = i.get_prop(&init, &pk("detail"))?;
            i.set_builtin(o, "detail", d);
        }
        i.set_builtin(o, "defaultPrevented", Value::Bool(false));
        i.set_builtin(o, "isTrusted", Value::Bool(false));
        i.set_builtin(o, "target", Value::Null);
        i.set_builtin(o, "currentTarget", Value::Null);
        i.set_builtin(o, K_STOP, Value::Bool(false));
        i.set_builtin(o, K_STOPIMM, Value::Bool(false));
        let ms = with_ds(i, |ds| ds.t0.elapsed().as_secs_f64() * 1000.0);
        i.set_builtin(o, "timeStamp", Value::Num(ms));
    }
    Ok(Value::Und)
}

fn event_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    event_init_from(i, this, args, false)
}

fn custom_event_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    event_init_from(i, this, args, true)
}

fn event_prevent_default(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    if let Value::Obj(o) = this {
        if i.to_bool(
            &i.obj_get_raw(o, &pk("cancelable"))
                .unwrap_or(Value::Bool(true)),
        ) {
            i.set_builtin(o, "defaultPrevented", Value::Bool(true));
        }
    }
    Ok(Value::Und)
}

fn event_stop_prop(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    if let Value::Obj(o) = this {
        i.set_builtin(o, K_STOP, Value::Bool(true));
    }
    Ok(Value::Und)
}

fn event_stop_imm(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    if let Value::Obj(o) = this {
        i.set_builtin(o, K_STOP, Value::Bool(true));
        i.set_builtin(o, K_STOPIMM, Value::Bool(true));
    }
    Ok(Value::Und)
}

fn event_init_event(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    if let Value::Obj(o) = this {
        let ty = i.to_str(&arg(args, 0))?;
        i.set_builtin(o, "type", Value::Str(Rc::from(ty.as_str())));
        i.set_builtin(o, "bubbles", Value::Bool(i.to_bool(&arg(args, 1))));
        i.set_builtin(o, "cancelable", Value::Bool(i.to_bool(&arg(args, 2))));
    }
    Ok(Value::Und)
}

fn event_composed_path(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    // target + propagation chain
    let target = i.get_prop(this, &pk("target"))?;
    let mut out = Vec::new();
    if let Some((did, nid)) = node_ref(i, &target) {
        out.push(target.clone());
        let anc = with_ds(i, |ds| {
            let mut v = Vec::new();
            let mut p = ds.docs[did as usize].nodes[nid].parent;
            while let Some(a) = p {
                v.push(a);
                p = ds.docs[did as usize].nodes[a].parent;
            }
            v
        });
        for a in anc {
            out.push(wrap_node(i, did, a));
        }
    }
    out.push(Value::Obj(i.p.global.clone()));
    Ok(Value::Obj(i.new_arr(out)))
}

// ---------------------------------------------------------------------------
// storage
// ---------------------------------------------------------------------------

fn store_map<'m>(ds: &'m mut DomState, which: &str) -> &'m mut Vec<(String, String)> {
    if which == "session" {
        &mut ds.storage_session
    } else {
        &mut ds.storage_local
    }
}

fn store_which(i: &Interp, this: &Value) -> String {
    if let Value::Obj(o) = this {
        if let Some(Value::Str(s)) = i.obj_get_raw(o, &pk(K_STORE)) {
            return s.to_string();
        }
    }
    "local".into()
}

fn st_get_item(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let k = i.to_str(&arg(args, 0))?;
    let w = store_which(i, this);
    Ok(with_ds(i, |ds| {
        match store_map(ds, &w).iter().find(|(k2, _)| *k2 == k) {
            Some((_, v)) => Value::Str(Rc::from(v.as_str())),
            None => Value::Null,
        }
    }))
}

fn st_set_item(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let k = i.to_str(&arg(args, 0))?;
    let v = i.to_str(&arg(args, 1))?;
    let w = store_which(i, this);
    with_ds(i, |ds| {
        let m = store_map(ds, &w);
        if let Some(e) = m.iter_mut().find(|(k2, _)| *k2 == k) {
            e.1 = v;
        } else {
            m.push((k, v));
        }
    });
    Ok(Value::Und)
}

fn st_remove_item(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let k = i.to_str(&arg(args, 0))?;
    let w = store_which(i, this);
    with_ds(i, |ds| store_map(ds, &w).retain(|(k2, _)| *k2 != k));
    Ok(Value::Und)
}

fn st_clear(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let w = store_which(i, this);
    with_ds(i, |ds| store_map(ds, &w).clear());
    Ok(Value::Und)
}

fn st_key(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let n = i.to_num(&arg(args, 0))? as usize;
    let w = store_which(i, this);
    Ok(with_ds(i, |ds| match store_map(ds, &w).get(n) {
        Some((k, _)) => Value::Str(Rc::from(k.as_str())),
        None => Value::Null,
    }))
}

fn st_length(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let w = store_which(i, this);
    Ok(Value::Num(with_ds(i, |ds| store_map(ds, &w).len()) as f64))
}

// ---------------------------------------------------------------------------
// window-ish globals
// ---------------------------------------------------------------------------

fn location_obj(i: &mut Interp) -> Value {
    if let Some(o) = with_ds(i, |ds| ds.location.clone()) {
        return Value::Obj(o);
    }
    let (proto, url) = (Some(i.p.obj.clone()), with_ds(i, |ds| ds.url.clone()));
    let o = obj(proto);
    let (mut protocol, mut host, mut path, mut search, mut hash) = (
        String::new(),
        String::new(),
        "/".to_string(),
        String::new(),
        String::new(),
    );
    // split scheme://authority/path?search#hash
    let mut rest = url.as_str();
    if let Some((scheme, r)) = rest.split_once("://") {
        protocol = format!("{scheme}:");
        rest = r;
        let (auth, p) = match rest.find('/') {
            Some(x) => (&rest[..x], &rest[x..]),
            None => (rest, ""),
        };
        host = auth.to_string();
        rest = p;
    }
    if let Some((r, h)) = rest.split_once('#') {
        rest = r;
        hash = format!("#{h}");
    }
    if let Some((r, s)) = rest.split_once('?') {
        rest = r;
        search = format!("?{s}");
    }
    if !rest.is_empty() {
        path = rest.to_string();
    }
    let hostname = host.split(':').next().unwrap_or("").to_string();
    let port = host.split(':').nth(1).unwrap_or("").to_string();
    let origin = if protocol.is_empty() {
        "null".to_string()
    } else if protocol == "file:" {
        "file://".to_string()
    } else {
        format!("{protocol}//{host}")
    };
    for (k, v) in [
        ("href", url.as_str()),
        ("origin", origin.as_str()),
        ("protocol", protocol.as_str()),
        ("host", host.as_str()),
        ("hostname", hostname.as_str()),
        ("port", port.as_str()),
        ("pathname", path.as_str()),
        ("search", search.as_str()),
        ("hash", hash.as_str()),
    ] {
        i.set_builtin(&o, k, Value::Str(Rc::from(v)));
    }
    for m in ["assign", "replace", "reload", "toString"] {
        i.set_builtin(&o, m, i.native(m, noop));
    }
    with_ds(i, |ds| ds.location = Some(o.clone()));
    Value::Obj(o)
}

fn perf_now(i: &mut Interp, _t: &Value, _a: &[Value]) -> NR {
    Ok(Value::Num(with_ds(i, |ds| {
        ds.t0.elapsed().as_secs_f64() * 1000.0
    })))
}

fn g_set_timeout(i: &mut Interp, _t: &Value, args: &[Value]) -> NR {
    let cb = arg(args, 0);
    let extra: Vec<Value> = args.iter().skip(2).cloned().collect();
    Ok(Value::Num(with_ds(i, |ds| {
        ds.next_timer += 1;
        let id = ds.next_timer;
        ds.timers.push_back(TimerJob {
            id,
            cb,
            args: extra,
            interval: false,
        });
        id as f64
    })))
}

fn g_set_interval(i: &mut Interp, _t: &Value, args: &[Value]) -> NR {
    let cb = arg(args, 0);
    let extra: Vec<Value> = args.iter().skip(2).cloned().collect();
    Ok(Value::Num(with_ds(i, |ds| {
        ds.next_timer += 1;
        let id = ds.next_timer;
        ds.timers.push_back(TimerJob {
            id,
            cb,
            args: extra,
            interval: true,
        });
        id as f64
    })))
}

fn g_clear_timer(i: &mut Interp, _t: &Value, args: &[Value]) -> NR {
    let id = i.to_num(&arg(args, 0))? as u64;
    with_ds(i, |ds| ds.timers.retain(|t| t.id != id));
    Ok(Value::Und)
}

fn g_raf(i: &mut Interp, _t: &Value, args: &[Value]) -> NR {
    let cb = arg(args, 0);
    Ok(Value::Num(with_ds(i, |ds| {
        ds.next_raf += 1;
        let id = ds.next_raf;
        ds.raf.push_back((id, cb));
        id as f64
    })))
}

fn g_cancel_raf(i: &mut Interp, _t: &Value, args: &[Value]) -> NR {
    let id = i.to_num(&arg(args, 0))? as u64;
    with_ds(i, |ds| ds.raf.retain(|(rid, _)| *rid != id));
    Ok(Value::Und)
}

fn g_queue_microtask(i: &mut Interp, _t: &Value, args: &[Value]) -> NR {
    let cb = arg(args, 0);
    with_ds(i, |ds| ds.microtasks.push_back((cb, vec![])));
    Ok(Value::Und)
}

fn g_idle_cb(i: &mut Interp, _t: &Value, args: &[Value]) -> NR {
    let cb = arg(args, 0);
    let deadline = i.new_obj();
    i.set_data(&deadline, "didTimeout", Value::Bool(false));
    i.set_builtin(
        &deadline,
        "timeRemaining",
        i.native("timeRemaining", idle_time),
    );
    Ok(Value::Num(with_ds(i, |ds| {
        ds.next_timer += 1;
        let id = ds.next_timer;
        ds.timers.push_back(TimerJob {
            id,
            cb,
            args: vec![Value::Obj(deadline)],
            interval: false,
        });
        id as f64
    })))
}

fn idle_time(_i: &mut Interp, _t: &Value, _a: &[Value]) -> NR {
    Ok(Value::Num(50.0))
}

fn g_match_media(i: &mut Interp, _t: &Value, args: &[Value]) -> NR {
    let q = i.to_str(&arg(args, 0))?;
    let o = i.new_obj();
    i.set_data(&o, "matches", Value::Bool(false));
    i.set_data(&o, "media", Value::Str(Rc::from(q.as_str())));
    i.set_data(&o, "onchange", Value::Null);
    for m in [
        "addEventListener",
        "removeEventListener",
        "addListener",
        "removeListener",
        "dispatchEvent",
    ] {
        i.set_builtin(&o, m, i.native(m, noop));
    }
    Ok(Value::Obj(o))
}

fn g_get_computed_style(i: &mut Interp, _t: &Value, args: &[Value]) -> NR {
    let el = arg(args, 0);
    let proto = with_ds(i, |ds| ds.p_compstyle.clone());
    let o = obj(Some(proto));
    i.set_builtin(&o, K_EL, el);
    Ok(Value::Obj(o))
}

fn cs_get_property(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Value::Obj(o) = this else {
        return Ok(Value::Str(Rc::from("")));
    };
    let k = i.to_str(&arg(args, 0))?;
    if let Some(el) = i.obj_get_raw(o, &pk(K_EL)) {
        if let Some((did, nid)) = node_ref(i, &el) {
            let v = with_ds(i, |ds| {
                ds.docs[did as usize]
                    .attr(nid, "style")
                    .unwrap_or("")
                    .split(';')
                    .filter_map(|d| d.split_once(':'))
                    .find(|(pk, _)| pk.trim() == k)
                    .map(|(_, v)| v.trim().to_string())
            });
            if let Some(v) = v {
                return Ok(Value::Str(Rc::from(v.as_str())));
            }
        }
    }
    Ok(Value::Str(Rc::from("")))
}

fn g_fetch(i: &mut Interp, _t: &Value, _a: &[Value]) -> NR {
    let e = i.plain_err("fetch is not supported in rowser");
    builtins::promise_reject_static(i, &Value::Und, &[e])
}

fn g_get_selection(i: &mut Interp, _t: &Value, _a: &[Value]) -> NR {
    let o = i.new_obj();
    for m in [
        "removeAllRanges",
        "addRange",
        "getRangeAt",
        "collapse",
        "extend",
        "empty",
        "setPosition",
    ] {
        i.set_builtin(&o, m, i.native(m, noop));
    }
    i.set_data(&o, "rangeCount", Value::Num(0.0));
    i.set_data(&o, "anchorNode", Value::Null);
    i.set_data(&o, "focusNode", Value::Null);
    i.set_data(&o, "isCollapsed", Value::Bool(true));
    Ok(Value::Obj(o))
}

// -- MutationObserver / ResizeObserver / IntersectionObserver -------------------

fn mo_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    if let Value::Obj(o) = this {
        i.set_builtin(o, K_CB, arg(args, 0));
    }
    Ok(Value::Und)
}

fn mo_observe(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    // Ping the callback with an empty records list on the next microtask —
    // hydration code that waits for the first callback won't deadlock.
    let Value::Obj(o) = this else {
        return Ok(Value::Und);
    };
    let cb = i.obj_get_raw(o, &pk(K_CB)).unwrap_or(Value::Und);
    if i.is_callable(&cb) {
        let records = Value::Obj(i.new_arr(vec![]));
        with_ds(i, |ds| {
            ds.microtasks.push_back((cb, vec![records, this.clone()]))
        });
    }
    Ok(Value::Und)
}

fn mo_take_records(i: &mut Interp, _t: &Value, _a: &[Value]) -> NR {
    Ok(Value::Obj(i.new_arr(vec![])))
}

// -- DOMParser ------------------------------------------------------------------

fn domparser_ctor(_i: &mut Interp, _t: &Value, _a: &[Value]) -> NR {
    Ok(Value::Und)
}

fn domparser_parse(i: &mut Interp, _t: &Value, args: &[Value]) -> NR {
    let s = i.to_str(&arg(args, 0))?;
    let did = with_ds(i, |ds| {
        ds.docs.push(rowser_html::parse(&s));
        (ds.docs.len() - 1) as u32
    });
    Ok(doc_obj(i, did))
}

// -- MessageChannel -------------------------------------------------------------

fn mc_ctor(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let _ = args;
    let Value::Obj(o) = this else {
        return Ok(Value::Und);
    };
    let mk = |i: &mut Interp| -> ObjRef {
        let p = i.new_obj();
        i.set_builtin(&p, "postMessage", i.native("postMessage", mc_post));
        i.set_builtin(&p, "start", i.native("start", noop));
        i.set_builtin(&p, "close", i.native("close", noop));
        i.set_builtin(
            &p,
            "addEventListener",
            i.native("addEventListener", el_add_event_listener),
        );
        i.set_builtin(
            &p,
            "removeEventListener",
            i.native("removeEventListener", el_remove_event_listener),
        );
        i.set_builtin(&p, "onmessage", Value::Null);
        i.set_builtin(&p, "onmessageerror", Value::Null);
        p
    };
    let p1 = mk(i);
    let p2 = mk(i);
    i.set_builtin(&p1, K_PEER, Value::Obj(p2.clone()));
    i.set_builtin(&p2, K_PEER, Value::Obj(p1.clone()));
    i.set_builtin(o, "port1", Value::Obj(p1));
    i.set_builtin(o, "port2", Value::Obj(p2));
    Ok(Value::Und)
}

fn mc_post(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Value::Obj(o) = this else {
        return Ok(Value::Und);
    };
    let msg = arg(args, 0);
    if let Some(Value::Obj(peer)) = i.obj_get_raw(o, &pk(K_PEER)) {
        with_ds(i, |ds| ds.port_msgs.push_back((peer, msg)));
    }
    // listeners registered via addEventListener('message', …) also fire —
    // the drain looks up onmessage + 'message' listeners.
    Ok(Value::Und)
}

// -- AbortController ------------------------------------------------------------

fn ac_ctor(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    let Value::Obj(o) = this else {
        return Ok(Value::Und);
    };
    let sig = i.new_obj();
    i.set_builtin(&sig, "aborted", Value::Bool(false));
    i.set_builtin(&sig, "reason", Value::Und);
    i.set_builtin(
        &sig,
        "addEventListener",
        i.native("addEventListener", el_add_event_listener),
    );
    i.set_builtin(
        &sig,
        "removeEventListener",
        i.native("removeEventListener", el_remove_event_listener),
    );
    i.set_builtin(
        &sig,
        "dispatchEvent",
        i.native("dispatchEvent", el_dispatch_event),
    );
    i.set_builtin(
        &sig,
        "throwIfAborted",
        i.native("throwIfAborted", sig_throw),
    );
    i.set_builtin(&sig, "onabort", Value::Null);
    i.set_builtin(o, "signal", Value::Obj(sig));
    Ok(Value::Und)
}

fn sig_throw(i: &mut Interp, this: &Value, _a: &[Value]) -> NR {
    if let Value::Obj(o) = this {
        if i.to_bool(
            &i.obj_get_raw(o, &pk("aborted"))
                .unwrap_or(Value::Bool(false)),
        ) {
            let r = i
                .obj_get_raw(o, &pk("reason"))
                .unwrap_or_else(|| i.plain_err("AbortError"));
            return Err(r);
        }
    }
    Ok(Value::Und)
}

fn ac_abort(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let Value::Obj(o) = this else {
        return Ok(Value::Und);
    };
    let reason = arg(args, 0);
    if let Some(Value::Obj(sig)) = i.obj_get_raw(o, &pk("signal")) {
        i.set_builtin(&sig, "aborted", Value::Bool(true));
        let r = if matches!(reason, Value::Und) {
            i.plain_err("AbortError: signal aborted")
        } else {
            reason
        };
        i.set_builtin(&sig, "reason", r);
        // fire 'abort' listeners synchronously
        let fns = listener_fns(i, &sig, "abort", false);
        let ev = make_event(i, "abort", false, false);
        let sv = Value::Obj(sig.clone());
        i.set_prop(&ev, pk("target"), sv.clone())?;
        for f in fns {
            i.call(&f, sv.clone(), vec![ev.clone()])?;
        }
    }
    Ok(Value::Und)
}

// ---------------------------------------------------------------------------
// registration
// ---------------------------------------------------------------------------

fn met(i: &Interp, o: &ObjRef, name: &str, f: NativeFn) {
    i.set_builtin(o, name, i.native(name, f));
}

fn acc(i: &Interp, o: &ObjRef, name: &str, get: Option<NativeFn>, set: Option<NativeFn>) {
    let g = get.map(|f| i.native(&format!("get {name}"), f));
    let s = set.map(|f| i.native(&format!("set {name}"), f));
    o.borrow_mut().props.insert(pk(name), Prop::accessor(g, s));
}

fn gdef(i: &mut Interp, name: &str, v: Value) {
    let g = i.global.clone();
    i.declare(&g, name, v, true);
}

fn dom_ctor(i: &mut Interp, name: &str, f: NativeFn, proto: &ObjRef) -> Value {
    let o = Rc::new(RefCell::new(Obj::new(
        Some(i.p.func.clone()),
        ObjKind::Func(Func::Native {
            name: Rc::from(name),
            f,
        }),
    )));
    i.set_builtin(&o, "name", Value::Str(Rc::from(name)));
    i.set_builtin(&o, "prototype", Value::Obj(proto.clone()));
    i.set_builtin(proto, "constructor", Value::Obj(o.clone()));
    Value::Obj(o)
}

fn elem_ctor_throw(i: &mut Interp, _t: &Value, _a: &[Value]) -> NR {
    Err(i.type_err("Illegal constructor"))
}

/// Build all host objects + globals on `i`. Takes ownership of `doc`.
pub fn install_dom(i: &mut Interp, doc: Document, url: &str, vw: f64, vh: f64) {
    // ---- prototype chain ----
    let p_node = obj(Some(i.p.obj.clone()));
    let p_elem = obj(Some(p_node.clone()));
    let p_html = obj(Some(p_elem.clone()));
    let p_svg = obj(Some(p_elem.clone()));
    let p_frag = obj(Some(p_node.clone()));
    let p_doc = obj(Some(p_node.clone()));
    let p_char = obj(Some(p_node.clone()));
    let p_event = obj(Some(i.p.obj.clone()));
    let p_style = obj(Some(i.p.obj.clone()));
    let p_classlist = obj(Some(i.p.obj.clone()));
    let p_storage = obj(Some(i.p.obj.clone()));
    let p_compstyle = obj(Some(i.p.obj.clone()));

    i.ext = Some(Box::new(DomState {
        docs: vec![doc],
        url: url.to_string(),
        t0: std::time::Instant::now(),
        nodes: HashMap::new(),
        tag_protos: HashMap::new(),
        svg_nodes: std::collections::HashSet::new(),
        doc_objs: HashMap::new(),
        p_html: p_html.clone(),
        p_svg: p_svg.clone(),
        p_frag: p_frag.clone(),
        p_doc: p_doc.clone(),
        p_char: p_char.clone(),
        p_event: p_event.clone(),
        p_style: p_style.clone(),
        p_classlist: p_classlist.clone(),
        p_storage: p_storage.clone(),
        p_compstyle: p_compstyle.clone(),
        timers: VecDeque::new(),
        next_timer: 0,
        raf: VecDeque::new(),
        next_raf: 0,
        microtasks: VecDeque::new(),
        port_msgs: VecDeque::new(),
        storage_local: Vec::new(),
        storage_session: Vec::new(),
        focused: None,
        location: None,
    }));

    // ---- Node.prototype ----
    for (n, f) in [
        ("appendChild", node_append_child as NativeFn),
        ("insertBefore", node_insert_before),
        ("removeChild", node_remove_child),
        ("replaceChild", node_replace_child),
        ("cloneNode", node_clone),
        ("contains", node_contains),
        ("hasChildNodes", node_has_children),
        ("remove", node_remove),
        ("getRootNode", node_get_root),
        ("append", node_append),
        ("prepend", node_prepend),
        ("before", node_before),
        ("after", node_after),
        ("replaceWith", node_replace_with),
        ("addEventListener", el_add_event_listener),
        ("removeEventListener", el_remove_event_listener),
        ("dispatchEvent", el_dispatch_event),
    ] {
        met(i, &p_node, n, f);
    }
    acc(i, &p_node, "childNodes", Some(node_child_nodes), None);
    acc(i, &p_node, "firstChild", Some(node_first_child), None);
    acc(i, &p_node, "lastChild", Some(node_last_child), None);
    acc(i, &p_node, "nextSibling", Some(node_next_sibling), None);
    acc(i, &p_node, "previousSibling", Some(node_prev_sibling), None);
    acc(i, &p_node, "parentNode", Some(node_parent), None);
    acc(i, &p_node, "parentElement", Some(node_parent_elem), None);
    acc(i, &p_node, "nodeType", Some(node_node_type), None);
    acc(i, &p_node, "nodeName", Some(node_node_name), None);
    acc(
        i,
        &p_node,
        "nodeValue",
        Some(node_node_value_get),
        Some(node_node_value_set),
    );
    acc(
        i,
        &p_node,
        "textContent",
        Some(node_text_content_get),
        Some(node_text_content_set),
    );
    acc(i, &p_node, "ownerDocument", Some(node_owner_document), None);
    acc(i, &p_node, "isConnected", Some(node_is_connected), None);
    acc(i, &p_node, "baseURI", Some(node_base_uri), None);

    // ---- Element.prototype ----
    for (n, f) in [
        ("getAttribute", el_get_attribute as NativeFn),
        ("getAttributeNS", el_get_attribute_ns),
        ("setAttribute", el_set_attribute),
        ("setAttributeNS", el_set_attribute_ns),
        ("removeAttribute", el_remove_attribute),
        ("removeAttributeNS", el_remove_attribute_ns),
        ("hasAttribute", el_has_attribute),
        ("hasAttributeNS", el_has_attribute_ns),
        ("toggleAttribute", el_toggle_attribute),
        ("getAttributeNames", el_attribute_names),
        ("getElementsByTagName", get_by_tag),
        ("getElementsByClassName", get_by_class),
        ("querySelector", query_selector),
        ("querySelectorAll", query_selector_all),
        ("matches", el_matches),
        ("closest", el_closest),
        ("insertAdjacentHTML", el_insert_adjacent_html),
        ("focus", el_focus),
        ("blur", el_blur),
        ("click", el_click),
        ("getBoundingClientRect", el_get_rect),
        ("getClientRects", el_get_rects),
        ("scrollIntoView", noop),
        ("releasePointerCapture", noop),
        ("setPointerCapture", noop),
        ("requestFullscreen", noop),
        ("scrollTo", noop),
        ("scrollBy", noop),
        ("animate", noop),
        ("attachShadow", noop),
    ] {
        met(i, &p_elem, n, f);
    }
    acc(i, &p_elem, "id", Some(el_id_get), Some(el_id_set));
    acc(
        i,
        &p_elem,
        "className",
        Some(el_classname_get),
        Some(el_classname_set),
    );
    acc(i, &p_elem, "tagName", Some(node_tag_name), None);
    acc(i, &p_elem, "localName", Some(el_local_name), None);
    acc(i, &p_elem, "namespaceURI", Some(el_namespace_uri), None);
    acc(i, &p_elem, "classList", Some(classlist_get), None);
    acc(i, &p_elem, "style", Some(style_get), None);
    acc(i, &p_elem, "dataset", Some(dataset_get), None);
    acc(i, &p_elem, "attributes", Some(el_attributes), None);
    acc(i, &p_elem, "children", Some(node_children), None);
    acc(i, &p_elem, "firstElementChild", Some(node_first_elem), None);
    acc(i, &p_elem, "lastElementChild", Some(node_last_elem), None);
    acc(i, &p_elem, "nextElementSibling", Some(node_next_elem), None);
    acc(
        i,
        &p_elem,
        "previousElementSibling",
        Some(node_prev_elem),
        None,
    );
    acc(
        i,
        &p_elem,
        "innerHTML",
        Some(el_inner_html_get),
        Some(el_inner_html_set),
    );
    acc(
        i,
        &p_elem,
        "outerHTML",
        Some(el_outer_html_get),
        Some(el_outer_html_set),
    );
    acc(
        i,
        &p_elem,
        "innerText",
        Some(node_text_content_get),
        Some(node_text_content_set),
    );
    // reflected attrs
    acc(i, &p_elem, "value", Some(val_get), Some(val_set));
    acc(i, &p_elem, "href", Some(href_get), Some(href_set));
    acc(i, &p_elem, "src", Some(src_get), Some(src_set));
    acc(i, &p_elem, "alt", Some(alt_get), Some(alt_set));
    acc(i, &p_elem, "name", Some(name_get), Some(name_set));
    acc(i, &p_elem, "type", Some(type_get), Some(type_set));
    acc(i, &p_elem, "placeholder", Some(ph_get), Some(ph_set));
    acc(i, &p_elem, "rel", Some(rel_get), Some(rel_set));
    acc(i, &p_elem, "lang", Some(lang_get), Some(lang_set));
    acc(i, &p_elem, "dir", Some(dir_get), Some(dir_set));
    acc(i, &p_elem, "htmlFor", Some(for_get), Some(for_set));
    acc(i, &p_elem, "action", Some(action_get), Some(action_set));
    acc(i, &p_elem, "method", Some(method_get), Some(method_set));
    acc(
        i,
        &p_elem,
        "title",
        Some(title_attr_get),
        Some(title_attr_set),
    );
    acc(i, &p_elem, "checked", Some(checked_get), Some(checked_set));
    acc(
        i,
        &p_elem,
        "disabled",
        Some(disabled_get),
        Some(disabled_set),
    );
    acc(
        i,
        &p_elem,
        "selected",
        Some(selected_get),
        Some(selected_set),
    );
    acc(
        i,
        &p_elem,
        "readOnly",
        Some(readonly_get),
        Some(readonly_set),
    );
    acc(
        i,
        &p_elem,
        "multiple",
        Some(multiple_get),
        Some(multiple_set),
    );
    acc(
        i,
        &p_elem,
        "required",
        Some(required_get),
        Some(required_set),
    );
    acc(
        i,
        &p_elem,
        "tabIndex",
        Some(tab_index_get),
        Some(tab_index_set),
    );
    // geometry — no layout here, all zeros
    for k in [
        "offsetWidth",
        "offsetHeight",
        "offsetTop",
        "offsetLeft",
        "clientWidth",
        "clientHeight",
        "clientTop",
        "clientLeft",
        "scrollWidth",
        "scrollHeight",
    ] {
        acc(i, &p_elem, k, Some(zero), None);
    }
    for k in ["scrollTop", "scrollLeft"] {
        acc(i, &p_elem, k, Some(zero), Some(noop));
    }
    i.set_builtin(&p_elem, "offsetParent", Value::Null);
    i.set_builtin(&p_elem, "slot", Value::Str(Rc::from("")));

    // ---- character-data prototype (Text/Comment) ----
    acc(i, &p_char, "data", Some(char_data_get), Some(char_data_set));
    acc(i, &p_char, "length", Some(char_length), None);
    met(i, &p_char, "splitText", text_split);
    met(i, &p_char, "substringData", noop);
    met(i, &p_char, "appendData", noop);
    met(i, &p_char, "deleteData", noop);
    met(i, &p_char, "insertData", noop);
    met(i, &p_char, "replaceData", noop);

    // ---- Document.prototype ----
    for (n, f) in [
        ("getElementById", doc_get_element_by_id as NativeFn),
        ("getElementsByTagName", get_by_tag),
        ("getElementsByClassName", get_by_class),
        ("getElementsByName", get_by_name),
        ("querySelector", query_selector),
        ("querySelectorAll", query_selector_all),
        ("createElement", doc_create_element),
        ("createElementNS", doc_create_element_ns),
        ("createTextNode", doc_create_text_node),
        ("createComment", doc_create_comment),
        ("createDocumentFragment", doc_create_fragment),
        ("createAttribute", doc_create_attribute),
        ("createEvent", doc_create_event),
        ("createRange", doc_create_range),
        ("importNode", doc_import_node),
        ("adoptNode", doc_import_node),
        ("hasFocus", doc_has_focus),
        ("execCommand", doc_exec_noop),
        ("open", noop),
        ("close", noop),
        ("write", noop),
        ("writeln", noop),
        ("getSelection", g_get_selection),
        ("caretPositionFromPoint", noop),
        ("elementFromPoint", noop),
    ] {
        met(i, &p_doc, n, f);
    }
    acc(i, &p_doc, "head", Some(doc_get_head), None);
    acc(i, &p_doc, "body", Some(doc_get_body), None);
    acc(i, &p_doc, "documentElement", Some(doc_get_docel), None);
    acc(i, &p_doc, "scrollingElement", Some(doc_get_docel), None);
    acc(i, &p_doc, "title", Some(doc_get_title), Some(doc_set_title));
    acc(i, &p_doc, "activeElement", Some(doc_active_element), None);
    acc(i, &p_doc, "links", Some(doc_links), None);
    acc(i, &p_doc, "forms", Some(doc_forms), None);
    acc(i, &p_doc, "images", Some(doc_images), None);
    acc(i, &p_doc, "scripts", Some(doc_scripts), None);
    acc(i, &p_doc, "children", Some(node_children), None);
    i.set_builtin(&p_doc, "hidden", Value::Bool(false));
    i.set_builtin(&p_doc, "visibilityState", Value::Str(Rc::from("visible")));
    i.set_builtin(&p_doc, "fullscreenElement", Value::Null);

    // ---- Event.prototype ----
    met(i, &p_event, "preventDefault", event_prevent_default);
    met(i, &p_event, "stopPropagation", event_stop_prop);
    met(i, &p_event, "stopImmediatePropagation", event_stop_imm);
    met(i, &p_event, "initEvent", event_init_event);
    met(i, &p_event, "composedPath", event_composed_path);

    // ---- classList / style / storage / computed-style protos ----
    met(i, &p_classlist, "add", cl_add);
    met(i, &p_classlist, "remove", cl_remove);
    met(i, &p_classlist, "toggle", cl_toggle);
    met(i, &p_classlist, "contains", cl_contains);
    met(i, &p_classlist, "toString", cl_to_string);
    met(i, &p_classlist, "item", cl_item);
    acc(i, &p_classlist, "value", Some(cl_to_string), None);
    acc(i, &p_classlist, "length", Some(cl_length), None);

    met(i, &p_style, "setProperty", style_set_property);
    met(i, &p_style, "getPropertyValue", style_get_property);
    met(i, &p_style, "removeProperty", style_remove_property);
    met(i, &p_style, "item", noop);

    met(i, &p_storage, "getItem", st_get_item);
    met(i, &p_storage, "setItem", st_set_item);
    met(i, &p_storage, "removeItem", st_remove_item);
    met(i, &p_storage, "clear", st_clear);
    met(i, &p_storage, "key", st_key);
    acc(i, &p_storage, "length", Some(st_length), None);

    met(i, &p_compstyle, "getPropertyValue", cs_get_property);
    met(i, &p_compstyle, "setProperty", noop);
    met(i, &p_compstyle, "removeProperty", noop);
    for k in STYLE_PROPS {
        if !p_compstyle.borrow().props.contains_key(&pk(k)) {
            p_compstyle
                .borrow_mut()
                .props
                .insert(pk(k), Prop::data(Value::Str(Rc::from(""))));
        }
    }

    // ---- constructors / globals ----
    let node_ctor = dom_ctor(i, "Node", elem_ctor_throw, &p_node);
    // Node statics
    for (k, v) in [
        ("ELEMENT_NODE", 1.0),
        ("ATTRIBUTE_NODE", 2.0),
        ("TEXT_NODE", 3.0),
        ("CDATA_SECTION_NODE", 4.0),
        ("PROCESSING_INSTRUCTION_NODE", 7.0),
        ("COMMENT_NODE", 8.0),
        ("DOCUMENT_NODE", 9.0),
        ("DOCUMENT_TYPE_NODE", 10.0),
        ("DOCUMENT_FRAGMENT_NODE", 11.0),
    ] {
        if let Value::Obj(o) = &node_ctor {
            i.set_builtin(o, k, Value::Num(v));
        }
    }
    gdef(i, "Node", node_ctor);
    for (name, proto, f) in [
        ("Element", &p_elem, elem_ctor_throw as NativeFn),
        ("HTMLElement", &p_html, elem_ctor_throw),
        ("SVGElement", &p_svg, elem_ctor_throw),
        ("DocumentFragment", &p_frag, elem_ctor_throw),
        ("Document", &p_doc, elem_ctor_throw),
        ("HTMLDocument", &p_doc, elem_ctor_throw),
        ("CharacterData", &p_char, elem_ctor_throw),
        ("Text", &p_char, elem_ctor_throw),
        ("Comment", &p_char, elem_ctor_throw),
        ("Event", &p_event, event_ctor),
        ("CustomEvent", &p_event, custom_event_ctor),
    ] {
        let v = dom_ctor(i, name, f, proto);
        gdef(i, name, v);
    }
    {
        let p = obj(Some(i.p.obj.clone()));
        met(i, &p, "observe", mo_observe);
        met(i, &p, "disconnect", noop);
        met(i, &p, "takeRecords", mo_take_records);
        let v = dom_ctor(i, "MutationObserver", mo_ctor, &p);
        gdef(i, "MutationObserver", v);
    }
    {
        let p = obj(Some(i.p.obj.clone()));
        met(i, &p, "parseFromString", domparser_parse);
        let v = dom_ctor(i, "DOMParser", domparser_ctor, &p);
        gdef(i, "DOMParser", v);
    }
    {
        let p = obj(Some(i.p.obj.clone()));
        let v = dom_ctor(i, "MessageChannel", mc_ctor, &p);
        gdef(i, "MessageChannel", v);
    }
    {
        let p = observer_proto(i);
        let v = dom_ctor(i, "ResizeObserver", mo_ctor, &p);
        gdef(i, "ResizeObserver", v);
        let p = observer_proto(i);
        let v = dom_ctor(i, "IntersectionObserver", mo_ctor, &p);
        gdef(i, "IntersectionObserver", v);
    }
    {
        let p = obj(Some(i.p.obj.clone()));
        met(i, &p, "abort", ac_abort);
        let v = dom_ctor(i, "AbortController", ac_ctor, &p);
        gdef(i, "AbortController", v);
    }
    {
        let p = obj(Some(i.p.obj.clone()));
        let v = dom_ctor(i, "AbortSignal", elem_ctor_throw, &p);
        gdef(i, "AbortSignal", v);
    }

    // Per-tag-family HTML element ctors — all "loose" (each has its own
    // prototype object chaining HTMLElement.prototype).
    for (tag, name) in [
        ("div", "HTMLDivElement"),
        ("span", "HTMLSpanElement"),
        ("a", "HTMLAnchorElement"),
        ("p", "HTMLParagraphElement"),
        ("input", "HTMLInputElement"),
        ("button", "HTMLButtonElement"),
        ("img", "HTMLImageElement"),
        ("canvas", "HTMLCanvasElement"),
        ("form", "HTMLFormElement"),
        ("label", "HTMLLabelElement"),
        ("select", "HTMLSelectElement"),
        ("option", "HTMLOptionElement"),
        ("textarea", "HTMLTextAreaElement"),
        ("ul", "HTMLUListElement"),
        ("ol", "HTMLOListElement"),
        ("li", "HTMLLIElement"),
        ("table", "HTMLTableElement"),
        ("tr", "HTMLTableRowElement"),
        ("td", "HTMLTableCellElement"),
        ("script", "HTMLScriptElement"),
        ("style", "HTMLStyleElement"),
        ("template", "HTMLTemplateElement"),
        ("iframe", "HTMLIFrameElement"),
        ("body", "HTMLBodyElement"),
        ("html", "HTMLHtmlElement"),
        ("head", "HTMLHeadElement"),
        ("title", "HTMLTitleElement"),
        ("br", "HTMLBRElement"),
        ("hr", "HTMLHRElement"),
        ("pre", "HTMLPreElement"),
        ("video", "HTMLVideoElement"),
        ("audio", "HTMLAudioElement"),
        ("meta", "HTMLMetaElement"),
        ("link", "HTMLLinkElement"),
        ("h", "HTMLHeadingElement"),
    ] {
        let proto = obj(Some(p_html.clone()));
        let ctor = dom_ctor(i, name, elem_ctor_throw, &proto);
        gdef(i, name, ctor);
        if tag != "h" {
            with_ds(i, |ds| {
                ds.tag_protos.insert(tag.to_string(), proto);
            });
        } else {
            for h in ["h1", "h2", "h3", "h4", "h5", "h6"] {
                with_ds(i, |ds| {
                    ds.tag_protos.insert(h.to_string(), proto.clone());
                });
            }
        }
    }
    {
        let p = obj(Some(p_html.clone()));
        let v = dom_ctor(i, "HTMLUnknownElement", elem_ctor_throw, &p);
        gdef(i, "HTMLUnknownElement", v);
    }

    // ---- the document + window surface ----
    let docv = doc_obj(i, 0);
    gdef(i, "document", docv.clone());
    let loc = location_obj(i);
    gdef(i, "location", loc);

    let g = Value::Obj(i.p.global.clone());
    for n in ["window", "self", "top", "parent", "frames"] {
        gdef(i, n, g.clone());
    }
    // globalThis is already defined by builtins

    // window.* props live on the global object
    {
        let go = i.p.global.clone();
        i.set_builtin(&go, "document", docv);
        i.set_builtin(&go, "innerWidth", Value::Num(vw));
        i.set_builtin(&go, "innerHeight", Value::Num(vh));
        i.set_builtin(&go, "outerWidth", Value::Num(vw));
        i.set_builtin(&go, "outerHeight", Value::Num(vh));
        i.set_builtin(&go, "devicePixelRatio", Value::Num(1.0));
        i.set_builtin(&go, "scrollX", Value::Num(0.0));
        i.set_builtin(&go, "scrollY", Value::Num(0.0));
        i.set_builtin(&go, "pageXOffset", Value::Num(0.0));
        i.set_builtin(&go, "pageYOffset", Value::Num(0.0));
        i.set_builtin(&go, "screenX", Value::Num(0.0));
        i.set_builtin(&go, "screenY", Value::Num(0.0));
        i.set_builtin(&go, "screenLeft", Value::Num(0.0));
        i.set_builtin(&go, "screenTop", Value::Num(0.0));
        i.set_builtin(
            &go,
            "addEventListener",
            i.native("addEventListener", el_add_event_listener),
        );
        i.set_builtin(
            &go,
            "removeEventListener",
            i.native("removeEventListener", el_remove_event_listener),
        );
        i.set_builtin(
            &go,
            "dispatchEvent",
            i.native("dispatchEvent", el_dispatch_event),
        );
        for m in [
            "scroll", "scrollTo", "scrollBy", "alert", "confirm", "prompt", "print", "blur",
            "focus", "close", "stop", "open",
        ] {
            i.set_builtin(&go, m, i.native(m, noop));
        }
        i.set_builtin(
            &go,
            "getComputedStyle",
            i.native("getComputedStyle", g_get_computed_style),
        );
        i.set_builtin(&go, "matchMedia", i.native("matchMedia", g_match_media));
        i.set_builtin(&go, "fetch", i.native("fetch", g_fetch));
        i.set_builtin(
            &go,
            "getSelection",
            i.native("getSelection", g_get_selection),
        );
        i.set_builtin(&go, "hasFocus", i.native("hasFocus", doc_has_focus));
    }

    // navigator / screen / history / performance / storage
    let nav = i.new_obj();
    i.set_builtin(
        &nav,
        "userAgent",
        Value::Str(Rc::from("Mozilla/5.0 (rowser) rowser-js")),
    );
    i.set_builtin(&nav, "appName", Value::Str(Rc::from("Netscape")));
    i.set_builtin(&nav, "appVersion", Value::Str(Rc::from("5.0 (rowser)")));
    i.set_builtin(&nav, "platform", Value::Str(Rc::from("Linux x86_64")));
    i.set_builtin(&nav, "language", Value::Str(Rc::from("en-US")));
    i.set_builtin(&nav, "vendor", Value::Str(Rc::from("rowser")));
    i.set_builtin(&nav, "product", Value::Str(Rc::from("Gecko")));
    i.set_builtin(&nav, "productSub", Value::Str(Rc::from("20100101")));
    i.set_builtin(&nav, "onLine", Value::Bool(true));
    i.set_builtin(&nav, "cookieEnabled", Value::Bool(true));
    i.set_builtin(&nav, "hardwareConcurrency", Value::Num(4.0));
    i.set_builtin(&nav, "maxTouchPoints", Value::Num(0.0));
    i.set_builtin(
        &nav,
        "languages",
        Value::Obj(i.new_arr(vec![Value::Str(Rc::from("en-US"))])),
    );
    gdef(i, "navigator", Value::Obj(nav));

    let screen = i.new_obj();
    for (k, v) in [
        ("width", vw),
        ("height", vh),
        ("availWidth", vw),
        ("availHeight", vh),
        ("colorDepth", 24.0),
        ("pixelDepth", 24.0),
    ] {
        i.set_builtin(&screen, k, Value::Num(v));
    }
    gdef(i, "screen", Value::Obj(screen));

    let hist = i.new_obj();
    i.set_builtin(&hist, "length", Value::Num(1.0));
    i.set_builtin(&hist, "state", Value::Null);
    i.set_builtin(&hist, "scrollRestoration", Value::Str(Rc::from("auto")));
    for m in ["pushState", "replaceState", "back", "forward", "go"] {
        i.set_builtin(&hist, m, i.native(m, noop));
    }
    gdef(i, "history", Value::Obj(hist));

    let perf = i.new_obj();
    i.set_builtin(&perf, "now", i.native("now", perf_now));
    i.set_builtin(&perf, "timeOrigin", Value::Num(0.0));
    i.set_builtin(&perf, "mark", i.native("mark", noop));
    i.set_builtin(&perf, "measure", i.native("measure", noop));
    i.set_builtin(&perf, "getEntries", i.native("getEntries", mo_take_records));
    i.set_builtin(
        &perf,
        "getEntriesByType",
        i.native("getEntriesByType", mo_take_records),
    );
    i.set_builtin(
        &perf,
        "getEntriesByName",
        i.native("getEntriesByName", mo_take_records),
    );
    gdef(i, "performance", Value::Obj(perf));

    // storage objects
    let (ls, ss) = {
        let mk = |i: &mut Interp, which: &str| -> Value {
            let proto = with_ds(i, |ds| ds.p_storage.clone());
            let o = obj(Some(proto));
            i.set_builtin(&o, K_STORE, Value::Str(Rc::from(which)));
            Value::Obj(o)
        };
        (mk(i, "local"), mk(i, "session"))
    };
    gdef(i, "localStorage", ls);
    gdef(i, "sessionStorage", ss);

    // timers / scheduling — override the inert builtins
    gdef(i, "setTimeout", i.native("setTimeout", g_set_timeout));
    gdef(i, "setInterval", i.native("setInterval", g_set_interval));
    gdef(i, "clearTimeout", i.native("clearTimeout", g_clear_timer));
    gdef(i, "clearInterval", i.native("clearInterval", g_clear_timer));
    gdef(
        i,
        "requestAnimationFrame",
        i.native("requestAnimationFrame", g_raf),
    );
    gdef(
        i,
        "cancelAnimationFrame",
        i.native("cancelAnimationFrame", g_cancel_raf),
    );
    gdef(
        i,
        "queueMicrotask",
        i.native("queueMicrotask", g_queue_microtask),
    );
    gdef(
        i,
        "requestIdleCallback",
        i.native("requestIdleCallback", g_idle_cb),
    );
    gdef(
        i,
        "cancelIdleCallback",
        i.native("cancelIdleCallback", g_clear_timer),
    );
}

fn observer_proto(i: &mut Interp) -> ObjRef {
    let p = obj(Some(i.p.obj.clone()));
    met(i, &p, "observe", noop);
    met(i, &p, "unobserve", noop);
    met(i, &p, "disconnect", noop);
    met(i, &p, "takeRecords", mo_take_records);
    p
}

fn cl_item(i: &mut Interp, this: &Value, args: &[Value]) -> NR {
    let n = i.to_num(&arg(args, 0))? as usize;
    let Some((_, _, toks)) = cl_tokens(i, this) else {
        return Ok(Value::Null);
    };
    Ok(match toks.get(n) {
        Some(t) => Value::Str(Rc::from(t.as_str())),
        None => Value::Null,
    })
}

// ---------------------------------------------------------------------------
// DomContext — public surface
// ---------------------------------------------------------------------------

/// A `Context` bound to a `Document`: JS globals (`document`, `window`,
/// timers…) plus the DOM host-object layer.
pub struct DomContext {
    pub ctx: Context,
    /// JS errors from event handlers / timers / microtasks (non-fatal).
    pub errors: Vec<String>,
}

impl DomContext {
    pub fn new(doc: Document, url: &str) -> DomContext {
        Self::with_viewport(doc, url, 1024.0, 768.0)
    }

    pub fn with_viewport(doc: Document, url: &str, vw: f64, vh: f64) -> DomContext {
        let mut ctx = Context::new();
        install_dom(&mut ctx.interp, doc, url, vw, vh);
        DomContext {
            ctx,
            errors: Vec::new(),
        }
    }

    /// Evaluate a script, then drain queued microtasks.
    pub fn eval(&mut self, src: &str) -> Result<Value, Error> {
        let r = self.ctx.eval(src);
        self.drain_microtasks();
        r
    }

    /// Run the microtask queue to quiescence (bounded).
    pub fn drain_microtasks(&mut self) {
        let i = &mut self.ctx.interp;
        for _ in 0..100_000usize {
            let job = with_ds(i, |ds| ds.microtasks.pop_front());
            let Some((cb, args)) = job else { break };
            if !i.is_callable(&cb) {
                continue;
            }
            if let Err(e) = i.call(&cb, Value::Und, args) {
                let msg = i.inspect(&e, 0);
                self.errors.push(format!("Uncaught (in microtask) {msg}"));
            }
        }
    }

    /// Drain the requestAnimationFrame queue once (timestamp arg in ms).
    pub fn flush_raf(&mut self) {
        let i = &mut self.ctx.interp;
        let (jobs, now) = with_ds(i, |ds| {
            (
                std::mem::take(&mut ds.raf),
                ds.t0.elapsed().as_secs_f64() * 1000.0,
            )
        });
        for (_, cb) in jobs {
            if !i.is_callable(&cb) {
                continue;
            }
            if let Err(e) = i.call(&cb, Value::Und, vec![Value::Num(now)]) {
                let msg = i.inspect(&e, 0);
                self.errors.push(format!("Uncaught (in raf) {msg}"));
            }
        }
        self.drain_microtasks();
    }

    /// Drain one round of pending timers + MessageChannel messages.
    pub fn run_timers(&mut self) {
        let i = &mut self.ctx.interp;
        let (jobs, msgs) = with_ds(i, |ds| {
            (
                std::mem::take(&mut ds.timers),
                std::mem::take(&mut ds.port_msgs),
            )
        });
        let mut requeue = Vec::new();
        for j in jobs {
            if i.is_callable(&j.cb) {
                if let Err(e) = i.call(&j.cb, Value::Und, j.args.clone()) {
                    let msg = i.inspect(&e, 0);
                    self.errors.push(format!("Uncaught (in timer) {msg}"));
                }
            }
            if j.interval {
                requeue.push(j);
            }
        }
        for (peer, msg) in msgs {
            let pv = Value::Obj(peer.clone());
            // onmessage handler
            if let Some(onm) = i.obj_get_raw(&peer, &pk("onmessage")) {
                if i.is_callable(&onm) {
                    let ev = i.new_obj();
                    i.set_builtin(&ev, "data", msg.clone());
                    i.set_builtin(&ev, "type", Value::Str(Rc::from("message")));
                    i.set_builtin(&ev, "target", pv.clone());
                    if let Err(e) = i.call(&onm, pv.clone(), vec![Value::Obj(ev)]) {
                        let m = i.inspect(&e, 0);
                        self.errors
                            .push(format!("Uncaught (in port onmessage) {m}"));
                    }
                }
            }
            // 'message' listeners
            for f in listener_fns(i, &peer, "message", false) {
                let ev = i.new_obj();
                i.set_builtin(&ev, "data", msg.clone());
                i.set_builtin(&ev, "type", Value::Str(Rc::from("message")));
                i.set_builtin(&ev, "target", pv.clone());
                if let Err(e) = i.call(&f, pv.clone(), vec![Value::Obj(ev)]) {
                    let m = i.inspect(&e, 0);
                    self.errors.push(format!("Uncaught (in port listener) {m}"));
                }
            }
        }
        with_ds(i, |ds| {
            for j in requeue {
                ds.timers.push_back(j);
            }
        });
        self.drain_microtasks();
    }

    /// Current document (immutable view).
    pub fn document(&self) -> &Document {
        let st = self
            .ctx
            .interp
            .ext
            .as_ref()
            .and_then(|b| b.downcast_ref::<DomState>())
            .expect("DomContext: no DomState");
        &st.docs[0]
    }

    /// Write JS-side property state (style props, dataset) back into attrs
    /// and hand the page Document out.
    pub fn take_document(&mut self) -> Document {
        sync_dom_side_effects(&mut self.ctx.interp);
        with_ds(&mut self.ctx.interp, |ds| {
            std::mem::replace(&mut ds.docs[0], Document::new())
        })
    }

    /// A snapshot of the live document (side effects synced first) — JS
    /// keeps mutating its own copy. Used by the renderer after each
    /// dispatched event.
    pub fn snapshot_document(&mut self) -> Document {
        sync_dom_side_effects(&mut self.ctx.interp);
        with_ds(&mut self.ctx.interp, |ds| ds.docs[0].clone())
    }

    /// Embedder-driven event: fire `ty` (e.g. "click") at the element
    /// `nid` in the page document with mouse coordinates. React-style
    /// delegated listeners see a bubbling event with a real target.
    pub fn dispatch_event(
        &mut self,
        nid: NodeId,
        ty: &str,
        client: (f64, f64),
    ) -> Result<bool, crate::Value> {
        let i = &mut self.ctx.interp;
        // detached/invalid nodes: dispatch on document itself
        let exists = with_ds(i, |ds| {
            ds.docs[0].nodes.get(nid).is_some()
                && !matches!(ds.docs[0].nodes[nid].kind, NodeKind::Document)
        });
        let target = if exists {
            wrap_node(i, 0, nid)
        } else {
            let root = ds_root(i);
            wrap_node(i, 0, root)
        };
        let ev = make_event(i, ty, true, true);
        i.set_prop(&ev, pk("target"), target.clone())?;
        i.set_prop(&ev, pk("srcElement"), target.clone())?;
        i.set_prop(&ev, pk("clientX"), Value::Num(client.0))?;
        i.set_prop(&ev, pk("clientY"), Value::Num(client.1))?;
        i.set_prop(&ev, pk("pageX"), Value::Num(client.0))?;
        i.set_prop(&ev, pk("pageY"), Value::Num(client.1))?;
        i.set_prop(&ev, pk("screenX"), Value::Num(client.0))?;
        i.set_prop(&ev, pk("screenY"), Value::Num(client.1))?;
        i.set_prop(&ev, pk("button"), Value::Num(0.0))?;
        i.set_prop(&ev, pk("buttons"), Value::Num(1.0))?;
        i.set_prop(&ev, pk("detail"), Value::Num(1.0))?;
        let r = run_event_path(i, &target, &ev)?;
        Ok(matches!(r, Value::Bool(true)))
    }
}

/// Root node of the page document (docs[0]).
fn ds_root(i: &mut Interp) -> NodeId {
    with_ds(i, |ds| ds.docs[0].root)
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(html: &str) -> DomContext {
        DomContext::new(rowser_html::parse(html), "file:///page.html")
    }

    fn run(dc: &mut DomContext, src: &str) -> Value {
        match dc.eval(src) {
            Ok(v) => v,
            Err(e) => panic!("eval failed for `{src}`: {e}"),
        }
    }

    fn s(dc: &mut DomContext, src: &str) -> String {
        let v = run(dc, src);
        dc.ctx
            .interp
            .to_str(&v)
            .unwrap_or_else(|_| "<unstringifiable>".to_string())
    }

    #[test]
    fn inner_html_roundtrip() {
        let mut dc = ctx("<html><body><div id='root'></div></body></html>");
        run(
            &mut dc,
            "document.getElementById('root').innerHTML = '<p>hi</p>'",
        );
        assert_eq!(
            s(&mut dc, "document.getElementById('root').innerHTML"),
            "<p>hi</p>"
        );
        assert_eq!(s(&mut dc, "document.querySelector('p').textContent"), "hi");
        // and the real Document was mutated
        let doc = dc.take_document();
        let root = doc
            .descendants(doc.root)
            .find(|&n| doc.attr(n, "id") == Some("root"))
            .unwrap();
        assert_eq!(doc.children(root).len(), 1);
        assert_eq!(doc.tag(doc.children(root)[0]), Some("p"));
    }

    #[test]
    fn query_selectors() {
        let mut dc = ctx(
            "<html><body><div id='a'><p class='x'>1</p><p>2</p><span class='x y'>3</span></div></body></html>",
        );
        assert_eq!(
            s(&mut dc, "document.querySelector('#a .x').textContent"),
            "1"
        );
        assert_eq!(s(&mut dc, "document.querySelectorAll('p').length"), "2");
        assert_eq!(s(&mut dc, "document.querySelectorAll('.x').length"), "2");
        assert_eq!(s(&mut dc, "document.querySelectorAll('.x.y').length"), "1");
        assert_eq!(s(&mut dc, "document.getElementsByTagName('p').length"), "2");
        assert_eq!(
            s(
                &mut dc,
                "document.getElementById('a').querySelector('span').className"
            ),
            "x y"
        );
        assert_eq!(
            s(
                &mut dc,
                "document.querySelector('div p:first-child').tagName"
            ),
            "P"
        );
    }

    #[test]
    fn text_content_and_attrs() {
        let mut dc = ctx("<html><body><div id='r'><b>a</b>c</div></body></html>");
        assert_eq!(s(&mut dc, "document.getElementById('r').textContent"), "ac");
        run(
            &mut dc,
            "var r = document.getElementById('r'); r.textContent = 'zz'",
        );
        assert_eq!(s(&mut dc, "r.childNodes.length"), "1");
        assert_eq!(s(&mut dc, "r.firstChild.nodeValue"), "zz");
        run(
            &mut dc,
            "r.setAttribute('data-x', '42'); r.setAttribute('title', 't')",
        );
        assert_eq!(s(&mut dc, "r.getAttribute('data-x')"), "42");
        assert_eq!(s(&mut dc, "r.hasAttribute('title')"), "true");
        run(&mut dc, "r.removeAttribute('title')");
        assert_eq!(s(&mut dc, "r.hasAttribute('title')"), "false");
    }

    #[test]
    fn create_and_insert() {
        let mut dc = ctx("<html><body><ul id='l'></ul></body></html>");
        run(
            &mut dc,
            r#"
            var l = document.getElementById('l');
            var a = document.createElement('li');
            a.appendChild(document.createTextNode('one'));
            l.appendChild(a);
            var b = document.createElement('li');
            b.textContent = 'two';
            l.appendChild(b);
            var c = document.createElement('li');
            c.textContent = 'zero';
            l.insertBefore(c, a);
            var frag = document.createDocumentFragment();
            var d = document.createElement('li');
            d.textContent = 'three';
            frag.appendChild(d);
            l.appendChild(frag);
            "#,
        );
        assert_eq!(s(&mut dc, "l.children.length"), "4");
        assert_eq!(s(&mut dc, "l.children[0].textContent"), "zero");
        assert_eq!(s(&mut dc, "l.children[3].tagName"), "LI");
        assert_eq!(
            s(
                &mut dc,
                "l.firstElementChild.nextElementSibling.textContent"
            ),
            "one"
        );
        run(&mut dc, "l.removeChild(a)");
        assert_eq!(s(&mut dc, "l.children.length"), "3");
        run(&mut dc, "l.replaceChild(document.createElement('em'), b)");
        assert_eq!(s(&mut dc, "l.children[1].tagName"), "EM");
        // cloneNode
        assert_eq!(
            s(&mut dc, "var q = l.cloneNode(true); q.children.length"),
            "3"
        );
        // instanceof
        assert_eq!(s(&mut dc, "l instanceof HTMLElement"), "true");
        assert_eq!(s(&mut dc, "l instanceof Element"), "true");
        assert_eq!(s(&mut dc, "l instanceof HTMLUListElement"), "true");
        assert_eq!(s(&mut dc, "l instanceof Node"), "true");
        assert_eq!(s(&mut dc, "l.nodeType"), "1");
        assert_eq!(s(&mut dc, "document.nodeType"), "9");
    }

    #[test]
    fn classlist_and_dataset() {
        let mut dc = ctx("<html><body><div id='r' class='a b' data-user='x'></div></body></html>");
        assert_eq!(
            s(
                &mut dc,
                "var r = document.getElementById('r'); r.classList.contains('a')"
            ),
            "true"
        );
        run(&mut dc, "r.classList.add('c'); r.classList.remove('a')");
        assert_eq!(s(&mut dc, "r.className"), "b c");
        assert_eq!(s(&mut dc, "r.classList.toggle('b')"), "false");
        assert_eq!(s(&mut dc, "r.classList.toggle('d', true)"), "true");
        assert_eq!(s(&mut dc, "r.className"), "c d");
        assert_eq!(s(&mut dc, "r.dataset.user"), "x");
        run(&mut dc, "r.dataset.newKey = 'v'");
        let doc = dc.take_document();
        let r = doc
            .descendants(doc.root)
            .find(|&n| doc.attr(n, "id") == Some("r"))
            .unwrap();
        assert_eq!(doc.attr(r, "class"), Some("c d"));
        assert_eq!(doc.attr(r, "data-new-key"), Some("v"));
    }

    #[test]
    fn style_syncs_to_attr() {
        let mut dc = ctx("<html><body><div id='r'></div></body></html>");
        run(&mut dc, "var r = document.getElementById('r'); r.style.display = 'none'; r.style.setProperty('color', 'red');");
        assert_eq!(
            s(&mut dc, "r.getAttribute('style')"),
            "display: none; color: red"
        );
        let doc = dc.take_document();
        let r = doc
            .descendants(doc.root)
            .find(|&n| doc.attr(n, "id") == Some("r"))
            .unwrap();
        let st = doc.attr(r, "style").unwrap_or("");
        assert!(st.contains("display: none"), "style={st}");
        assert!(st.contains("color: red"), "style={st}");
    }

    #[test]
    fn events_fire_and_bubble() {
        let mut dc = ctx("<html><body><div id='p'><button id='b'></button></div></body></html>");
        run(
            &mut dc,
            r#"
            var hits = [];
            var b = document.getElementById('b');
            var p = document.getElementById('p');
            b.addEventListener('click', function(e) { hits.push('b:' + e.type + ':' + (e.target === b)); });
            p.addEventListener('click', function(e) { hits.push('p:' + (e.currentTarget === p)); });
            document.addEventListener('click', function(e) { hits.push('doc'); });
            b.dispatchEvent(new Event('click', {bubbles: true}));
            "#,
        );
        assert_eq!(s(&mut dc, "hits.join('|')"), "b:click:true|p:true|doc");
        // stopPropagation
        run(
            &mut dc,
            r#"
            hits.length = 0;
            b.addEventListener('click', function(e) { e.stopPropagation(); });
            b.dispatchEvent(new Event('click', {bubbles: true}));
            "#,
        );
        assert_eq!(s(&mut dc, "hits.join('|')"), "b:click:true");
        // preventDefault makes dispatchEvent return false
        run(
            &mut dc,
            "p.addEventListener('x', function(e){ e.preventDefault(); }); var rv = p.dispatchEvent(new Event('x', {cancelable: true}));",
        );
        assert_eq!(s(&mut dc, "rv"), "false");
        // click() dispatches a click event
        run(&mut dc, "hits.length = 0; b.click()");
        assert!(s(&mut dc, "hits.length").starts_with('1') || s(&mut dc, "hits.length") == "2");
    }

    #[test]
    fn timers_raf_microtasks() {
        let mut dc = ctx("<html><body></body></html>");
        run(
            &mut dc,
            r#"
            var log = [];
            setTimeout(function(a) { log.push('t' + a); }, 0, '1');
            requestAnimationFrame(function(ts) { log.push('raf'); });
            queueMicrotask(function() { log.push('mt'); });
            log.push('end');
            "#,
        );
        // microtask ran after eval; timers/raf pending
        assert_eq!(s(&mut dc, "log.join(',')"), "end,mt");
        dc.flush_raf();
        dc.run_timers();
        assert_eq!(s(&mut dc, "log.join(',')"), "end,mt,raf,t1");
    }

    #[test]
    fn storage_and_location() {
        let mut dc = ctx("<html><body></body></html>");
        run(
            &mut dc,
            "localStorage.setItem('k', 'v'); localStorage.setItem('k2', 'w');",
        );
        assert_eq!(s(&mut dc, "localStorage.getItem('k')"), "v");
        assert_eq!(s(&mut dc, "localStorage.length"), "2");
        assert_eq!(s(&mut dc, "localStorage.key(1)"), "k2");
        run(&mut dc, "localStorage.removeItem('k')");
        assert_eq!(s(&mut dc, "localStorage.getItem('k')"), "null");
        assert_eq!(s(&mut dc, "location.pathname"), "/page.html");
        assert_eq!(s(&mut dc, "location.protocol"), "file:");
        assert_eq!(s(&mut dc, "location.origin"), "file://");
        assert_eq!(s(&mut dc, "navigator.language"), "en-US");
        assert_eq!(s(&mut dc, "document.readyState"), "complete");
        assert_eq!(s(&mut dc, "window === self && self === globalThis"), "true");
        assert_eq!(s(&mut dc, "document.defaultView === window"), "true");
    }

    #[test]
    fn nested_inner_html() {
        let mut dc = ctx("<html><body><div id='r'></div></body></html>");
        run(
            &mut dc,
            "document.getElementById('r').innerHTML = '<section><h2 class=\"t\">T</h2><p>x<b>y</b>z</p></section>'",
        );
        assert_eq!(
            s(&mut dc, "document.querySelector('h2.t').textContent"),
            "T"
        );
        assert_eq!(
            s(&mut dc, "document.querySelector('section p').textContent"),
            "xyz"
        );
        assert_eq!(
            s(&mut dc, "document.querySelector('#r').innerHTML"),
            "<section><h2 class=\"t\">T</h2><p>x<b>y</b>z</p></section>"
        );
    }

    #[test]
    fn mini_spa_mount_flow() {
        // React-shape smoke: delegated root listener, fragment mount,
        // timer-scheduled state update, attr serialization.
        let mut dc =
            ctx("<html><head><title>t</title></head><body><div id='root'></div></body></html>");
        run(
            &mut dc,
            r#"
            var root = document.getElementById('root');
            var clicks = 0;
            root.addEventListener('click', function(e) {
                var t = e.target;
                if (t && t.matches && t.matches('button')) clicks++;
            });
            function h(tag, cls, txt) {
                var el = document.createElement(tag);
                if (cls) el.className = cls;
                if (txt != null) el.appendChild(document.createTextNode(txt));
                return el;
            }
            var app = document.createDocumentFragment();
            app.appendChild(h('h1', 'title', 'My App'));
            var btn = h('button', 'btn', 'Click me');
            btn.setAttribute('data-id', '1');
            app.appendChild(btn);
            root.appendChild(app);
            setTimeout(function() {
                btn.textContent = 'Clicked!';
                btn.classList.add('done');
            }, 0);
            "#,
        );
        assert_eq!(
            s(&mut dc, "root.querySelector('h1.title').textContent"),
            "My App"
        );
        dc.run_timers();
        run(
            &mut dc,
            "btn.dispatchEvent(new Event('click', {bubbles: true}))",
        );
        assert_eq!(s(&mut dc, "clicks"), "1");
        assert_eq!(
            s(&mut dc, "root.innerHTML"),
            "<h1 class=\"title\">My App</h1><button class=\"btn done\" data-id=\"1\">Clicked!</button>"
        );
        let doc = dc.take_document();
        let btn = doc
            .descendants(doc.root)
            .find(|&n| doc.attr(n, "data-id") == Some("1"))
            .unwrap();
        assert_eq!(doc.attr(btn, "class"), Some("btn done"));
        assert_eq!(doc.text_content(btn), "Clicked!");
    }

    #[test]
    fn mutation_observer_ping_and_domparser() {
        let mut dc = ctx("<html><body><div id='r'></div></body></html>");
        run(
            &mut dc,
            r#"
            var fired = 0;
            var mo = new MutationObserver(function(recs, obs) { fired++; });
            mo.observe(document.getElementById('r'), {childList: true, subtree: true});
            var dp = new DOMParser();
            var d2 = dp.parseFromString('<html><body><p id="q">hi</p></body></html>', 'text/html');
            var qtxt = d2.getElementById('q').textContent;
            "#,
        );
        assert_eq!(s(&mut dc, "fired"), "1");
        assert_eq!(s(&mut dc, "qtxt"), "hi");
    }
}
