//! rowser-dom: the document object model tree.
//!
//! Arena-based: nodes live in a flat `Vec`, referenced by `NodeId`. Cheap to
//! build (parser appends), cheap to traverse (parent/child/sibling links).

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

pub type NodeId = usize;

/// rustc-hash-style hasher: one multiply-rotate per 8 bytes instead of
/// SipHash. `attr()` is probed millions of times during selector
/// matching and layout — the default hasher's per-call cost dominated
/// the lookup itself, and attr names are short ASCII keys where this
/// is plenty.
#[doc(hidden)]
#[derive(Default)]
pub struct FxHasher {
    hash: u64,
}

impl FxHasher {
    const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;
    #[inline]
    fn add(&mut self, v: u64) {
        self.hash = (self.hash.rotate_left(5) ^ v).wrapping_mul(Self::SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.add(u64::from_ne_bytes(c.try_into().unwrap()));
        }
        for &b in chunks.remainder() {
            self.add(u64::from(b));
        }
    }
    #[inline]
    fn write_u8(&mut self, v: u8) {
        self.add(u64::from(v));
    }
    #[inline]
    fn write_u16(&mut self, v: u16) {
        self.add(u64::from(v));
    }
    #[inline]
    fn write_u32(&mut self, v: u32) {
        self.add(u64::from(v));
    }
    #[inline]
    fn write_u64(&mut self, v: u64) {
        self.add(v);
    }
    #[inline]
    fn write_usize(&mut self, v: usize) {
        self.add(v as u64);
    }
}

/// Element attribute map — a std `HashMap` with the fast hasher above.
/// Behaves like `HashMap<String, String>` for every read/write pattern
/// used across the workspace (`get`, `insert`, `remove`, `iter`,
/// `keys`, `into_iter`).
pub type AttrMap = HashMap<String, String, BuildHasherDefault<FxHasher>>;

#[derive(Debug, Clone)]
pub enum NodeKind {
    Document,
    /// tag is lowercase; attrs preserve first-seen value per name.
    Element { tag: String, attrs: AttrMap },
    Text(String),
    Comment(String),
}

#[derive(Debug, Clone)]
pub struct Node {
    pub kind: NodeKind,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    /// Next sibling index within parent, or usize::MAX.
    pub sibling_index: usize,
}

#[derive(Debug, Clone)]
pub struct Document {
    pub nodes: Vec<Node>,
    pub root: NodeId,
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

impl Document {
    pub fn new() -> Self {
        Self {
            nodes: vec![Node {
                kind: NodeKind::Document,
                parent: None,
                children: Vec::new(),
                sibling_index: 0,
            }],
            root: 0,
        }
    }

    pub fn create_element(&mut self, tag: &str, attrs: Vec<(String, String)>) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(Node {
            kind: NodeKind::Element {
                tag: tag.to_lowercase(),
                attrs: attrs.into_iter().collect(),
            },
            parent: None,
            children: Vec::new(),
            sibling_index: usize::MAX,
        });
        id
    }

    pub fn create_text(&mut self, text: &str) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(Node {
            kind: NodeKind::Text(text.to_string()),
            parent: None,
            children: Vec::new(),
            sibling_index: usize::MAX,
        });
        id
    }

    pub fn create_comment(&mut self, text: &str) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(Node {
            kind: NodeKind::Comment(text.to_string()),
            parent: None,
            children: Vec::new(),
            sibling_index: usize::MAX,
        });
        id
    }

    /// Append `child` under `parent`, fixing parent/sibling links.
    pub fn append(&mut self, parent: NodeId, child: NodeId) {
        // detach if it had a parent
        if let Some(p) = self.nodes[child].parent {
            self.nodes[p].children.retain(|&c| c != child);
        }
        let idx = self.nodes[parent].children.len();
        self.nodes[parent].children.push(child);
        self.nodes[child].parent = Some(parent);
        self.nodes[child].sibling_index = idx;
    }

    #[inline]
    pub fn kind(&self, id: NodeId) -> &NodeKind {
        &self.nodes[id].kind
    }

    #[inline]
    pub fn tag(&self, id: NodeId) -> Option<&str> {
        match &self.nodes[id].kind {
            NodeKind::Element { tag, .. } => Some(tag),
            _ => None,
        }
    }

    #[inline]
    pub fn attr(&self, id: NodeId, name: &str) -> Option<&str> {
        match &self.nodes[id].kind {
            // empty-map early out: hashbrown hashes `name` before it can
            // see the table is empty, and most probed names ("id",
            // "class", pseudo hooks) are absent — skip the hash entirely.
            NodeKind::Element { attrs, .. } if !attrs.is_empty() => {
                attrs.get(name).map(String::as_str)
            }
            _ => None,
        }
    }

    pub fn set_attr(&mut self, id: NodeId, name: &str, val: &str) {
        if let NodeKind::Element { attrs, .. } = &mut self.nodes[id].kind {
            attrs.insert(name.to_string(), val.to_string());
        }
    }

    #[inline]
    pub fn text(&self, id: NodeId) -> Option<&str> {
        match &self.nodes[id].kind {
            NodeKind::Text(t) => Some(t),
            _ => None,
        }
    }

    #[inline]
    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.nodes[id].parent
    }

    #[inline]
    pub fn children(&self, id: NodeId) -> &[NodeId] {
        &self.nodes[id].children
    }

    /// Depth-first traversal of a subtree, source order.
    #[inline]
    pub fn descendants(&self, from: NodeId) -> Descendants<'_> {
        let mut it = Descendants {
            doc: self,
            inline: [0; DFS_INLINE],
            len: 0,
            spill: Vec::new(),
        };
        for &c in self.nodes[from].children.iter().rev() {
            it.push(c);
        }
        it
    }

    /// Concatenated text of a subtree (script/style/comments excluded).
    pub fn text_content(&self, id: NodeId) -> String {
        let mut out = String::new();
        self.text_into(id, &mut out);
        out
    }

    fn text_into(&self, id: NodeId, out: &mut String) {
        match &self.nodes[id].kind {
            NodeKind::Text(t) => out.push_str(t),
            NodeKind::Element { tag, .. } if tag == "script" || tag == "style" => {}
            _ => {
                for &c in &self.nodes[id].children {
                    self.text_into(c, out);
                }
            }
        }
    }

    /// First element child with the given tag (e.g. "body").
    #[inline]
    pub fn find(&self, from: NodeId, tag: &str) -> Option<NodeId> {
        self.nodes[from]
            .children
            .iter()
            .find(|&&c| self.tag(c) == Some(tag))
            .copied()
    }

    /// Depth-first search for first element with tag.
    pub fn find_deep(&self, from: NodeId, tag: &str) -> Option<NodeId> {
        for &c in &self.nodes[from].children {
            if self.tag(c) == Some(tag) {
                return Some(c);
            }
            if let Some(found) = self.find_deep(c, tag) {
                return Some(found);
            }
        }
        None
    }
}

/// Live DFS-stack capacity held inside `Descendants` — covers typical
/// pending-sibling depth without touching the heap.
const DFS_INLINE: usize = 32;

/// Depth-first iterator over a node's subtree.
///
/// The DFS stack lives inline (`DFS_INLINE` ids) with a `Vec` spill —
/// `descendants()` used to malloc a `Vec` per call plus a push/pop per
/// node, and selector matching + layout call it per candidate element.
/// For subtrees whose pending-sibling stack stays under `DFS_INLINE`
/// there is no heap traffic at all.
pub struct Descendants<'a> {
    doc: &'a Document,
    inline: [NodeId; DFS_INLINE],
    /// live entries at the top of `inline` (stack top = `len - 1`)
    len: usize,
    /// entries pushed while `inline` was full; drains before `inline`
    spill: Vec<NodeId>,
}

impl<'a> Descendants<'a> {
    #[inline]
    fn push(&mut self, v: NodeId) {
        if self.spill.is_empty() && self.len < DFS_INLINE {
            self.inline[self.len] = v;
            self.len += 1;
        } else {
            self.spill.push(v);
        }
    }

    #[inline]
    fn pop(&mut self) -> Option<NodeId> {
        if let Some(v) = self.spill.pop() {
            return Some(v);
        }
        if self.len > 0 {
            self.len -= 1;
            Some(self.inline[self.len])
        } else {
            None
        }
    }
}

impl<'a> Iterator for Descendants<'a> {
    type Item = NodeId;
    #[inline]
    fn next(&mut self) -> Option<NodeId> {
        let id = self.pop()?;
        for &c in self.doc.nodes[id].children.iter().rev() {
            self.push(c);
        }
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_tree() {
        let mut d = Document::new();
        let html = d.create_element("html", vec![]);
        let p = d.create_element("p", vec![("class".into(), "x".into())]);
        let t = d.create_text("hi");
        d.append(d.root, html);
        d.append(html, p);
        d.append(p, t);
        assert_eq!(d.text_content(html), "hi");
        assert_eq!(d.attr(p, "class"), Some("x"));
        assert_eq!(d.parent(p), Some(html));
        assert_eq!(d.find(html, "p"), Some(p));
    }
}
