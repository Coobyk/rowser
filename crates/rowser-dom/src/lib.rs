//! rowser-dom: the document object model tree.
//!
//! Arena-based: nodes live in a flat `Vec`, referenced by `NodeId`. Cheap to
//! build (parser appends), cheap to traverse (parent/child/sibling links).

use std::collections::HashMap;

pub type NodeId = usize;

#[derive(Debug, Clone)]
pub enum NodeKind {
    Document,
    /// tag is lowercase; attrs preserve first-seen value per name.
    Element {
        tag: String,
        attrs: HashMap<String, String>,
    },
    Text(String),
    Comment(String),
}

#[derive(Debug)]
pub struct Node {
    pub kind: NodeKind,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    /// Next sibling index within parent, or usize::MAX.
    pub sibling_index: usize,
}

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

    pub fn kind(&self, id: NodeId) -> &NodeKind {
        &self.nodes[id].kind
    }

    pub fn tag(&self, id: NodeId) -> Option<&str> {
        match &self.nodes[id].kind {
            NodeKind::Element { tag, .. } => Some(tag),
            _ => None,
        }
    }

    pub fn attr(&self, id: NodeId, name: &str) -> Option<&str> {
        match &self.nodes[id].kind {
            NodeKind::Element { attrs, .. } => attrs.get(name).map(String::as_str),
            _ => None,
        }
    }

    pub fn text(&self, id: NodeId) -> Option<&str> {
        match &self.nodes[id].kind {
            NodeKind::Text(t) => Some(t),
            _ => None,
        }
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.nodes[id].parent
    }

    pub fn children(&self, id: NodeId) -> &[NodeId] {
        &self.nodes[id].children
    }

    /// Depth-first traversal of element nodes.
    pub fn descendants(&self, from: NodeId) -> Descendants<'_> {
        Descendants {
            doc: self,
            stack: self.nodes[from].children.iter().rev().copied().collect(),
        }
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

pub struct Descendants<'a> {
    doc: &'a Document,
    stack: Vec<NodeId>,
}

impl<'a> Iterator for Descendants<'a> {
    type Item = NodeId;
    fn next(&mut self) -> Option<NodeId> {
        let id = self.stack.pop()?;
        for &c in self.doc.nodes[id].children.iter().rev() {
            self.stack.push(c);
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
