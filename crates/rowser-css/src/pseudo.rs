//! Pseudo-element style resolution.
//!
//! Selectors whose rightmost compound carries a `::pseudo-element`
//! ([`Simple::PseudoElem`]) never match a real element — instead
//! [`StyleCtx::computed_pseudo`] computes the style of an element's
//! generated box for a given kind. The element itself must still match
//! the rest of the selector: the pseudo is stripped from the rightmost
//! compound for matching (an emptied compound acts as `*`, so `::before`
//! means `*::before`, and `div ::before` means `div *::before`).

use super::*;

impl<'a> StyleCtx<'a> {
    /// Computed style for `node`'s `kind` pseudo-element box.
    ///
    /// Same cascade as [`StyleCtx::computed`]: origin/layer/specificity/
    /// order via `decl_wins`, shorthand expansion via `each_decl`. There
    /// is no inline-style contribution (pseudo boxes have no element to
    /// carry a `style` attribute); custom properties resolve against the
    /// pseudo rules' own `--*` decls, then the element's, then :root's.
    pub fn computed_pseudo(&mut self, node: NodeId, kind: PseudoElem) -> StrMap<String> {
        let doc = self.doc;
        let key = node_key(doc, node);
        // value + ordering key: (important, origin, layer, spec, order)
        let mut out: StrMap<(String, bool, u8, u32, (u32, u32, u32), usize)> = StrMap::default();
        let mut customs: StrMap<String> = StrMap::default();
        let mut order = 0usize;
        for (origin, sheet) in self.sheets.iter().enumerate() {
            sheet.candidates_pseudo_into(doc, node, kind, &mut self.buf);
            for &ri in &self.buf {
                let rule = &sheet.rules[ri as usize];
                let layer = sheet.layers.get(ri as usize).copied().unwrap_or(u32::MAX);
                let mut spec = (0u32, 0u32, 0u32);
                let mut matched = false;
                for (si, s) in rule.selectors.iter().enumerate() {
                    if !sel_has_pseudo(s, kind) {
                        continue;
                    }
                    if rightmost_ok(&key, s) && subject_matches(doc, node, s, &key) {
                        matched = true;
                        let sp = cascade::rule_spec(sheet, ri, si, s);
                        if sp > spec {
                            spec = sp;
                        }
                    }
                }
                if !matched {
                    continue;
                }
                for d in &rule.decls {
                    if d.prop.starts_with("--") {
                        customs.insert(d.prop.clone(), d.value.clone());
                    }
                    let nk = (d.important, origin as u8, layer, spec, order);
                    each_decl(&d.prop, &d.value, |prop, val| match out.get_mut(prop) {
                        Some(e) => {
                            if decl_wins(nk, (e.1, e.2, e.3, e.4, e.5)) {
                                *e = (
                                    val.to_string(),
                                    d.important,
                                    origin as u8,
                                    layer,
                                    spec,
                                    order,
                                );
                            }
                        }
                        None => {
                            out.insert(
                                prop.to_string(),
                                (
                                    val.to_string(),
                                    d.important,
                                    origin as u8,
                                    layer,
                                    spec,
                                    order,
                                ),
                            );
                        }
                    });
                }
                order += 1;
            }
        }
        let mut props: StrMap<String> = out
            .into_iter()
            .map(|(k, (v, _, _, _, _, _))| (k, v))
            .collect();
        // var() against: pseudo decls' customs → element's → :root's
        if props.values().any(|v| v.contains("var(")) {
            for (k, v) in element_customs(doc, node, &self.sheets) {
                customs.entry(k).or_insert(v);
            }
            let root = self.root_customs();
            for (k, v) in root.iter() {
                customs.entry(k.clone()).or_insert_with(|| v.clone());
            }
            for v in props.values_mut() {
                *v = resolve_vars(v, &customs);
            }
        }
        props
    }
}

/// Does `sel`'s rightmost compound carry `::kind`?
fn sel_has_pseudo(sel: &Selector, kind: PseudoElem) -> bool {
    sel.parts.last().is_some_and(|(_, last)| {
        last.iter()
            .any(|s| matches!(s, Simple::PseudoElem(k) if *k == kind))
    })
}

/// Element-side match for a `::kind` selector: the rightmost compound
/// with pseudo-elements ignored (they're checked by `sel_has_pseudo`;
/// an emptied compound acts as `*`), plus the usual leftward walk —
/// what `matches` would do on a stripped selector, without cloning it.
fn subject_matches(doc: &Document, node: NodeId, sel: &Selector, key: &NodeKey<'_>) -> bool {
    let n = sel.parts.len();
    if n == 0 {
        return false;
    }
    let st = &PseudoState::default();
    let (_, last) = &sel.parts[n - 1];
    if !last.iter().all(|s| match s {
        Simple::PseudoElem(_) => true,
        Simple::Tag(t) => key.tag == Some(t.as_str()),
        Simple::Class(c) => key
            .class
            .is_some_and(|a| a.split_whitespace().any(|x| x == c)),
        Simple::Id(i) => key.id == Some(i.as_str()),
        _ => simple_matches(doc, node, s, st),
    }) {
        return false;
    }
    rest_matches(doc, node, &sel.parts, st)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rowser_dom::Document;

    fn doc(html: &str) -> Document {
        rowser_html::parse(html)
    }

    fn sel(css: &str) -> Selector {
        let mut sh = parse(&format!("{css} {{ k: v }}"));
        sh.rules.remove(0).selectors.remove(0)
    }

    #[test]
    fn parses_pseudo_elements() {
        for (css, kind) in [
            ("p::before", PseudoElem::Before),
            ("p::after", PseudoElem::After),
            ("li::marker", PseudoElem::Marker),
            ("p::first-letter", PseudoElem::FirstLetter),
            ("p::first-line", PseudoElem::FirstLine),
            ("p::selection", PseudoElem::Selection),
            ("input::placeholder", PseudoElem::Placeholder),
            // CSS2 single-colon spellings for the four legacy pseudos
            ("p:before", PseudoElem::Before),
            ("p:after", PseudoElem::After),
            ("p:first-letter", PseudoElem::FirstLetter),
            ("p:first-line", PseudoElem::FirstLine),
        ] {
            let s = sel(css);
            let (_, last) = s.parts.last().unwrap();
            assert!(
                last.iter()
                    .any(|s| matches!(s, Simple::PseudoElem(k) if *k == kind)),
                "{css} should carry {kind:?}: {s:?}"
            );
        }
        // unknown pseudo-elements still parse as never-matching
        let s = sel("p::-webkit-scrollbar");
        let (_, last) = s.parts.last().unwrap();
        assert!(last.iter().any(|s| matches!(s, Simple::Never)));
    }

    #[test]
    fn pseudo_never_matches_element() {
        let d = doc("<body><p></p></body>");
        let p = d.find_deep(d.root, "p").unwrap();
        for s in ["p::before", "p::marker", "p::selection", "p::first-line"] {
            assert!(!matches(&d, p, &sel(s)), "{s} must not match elements");
        }
        // a rule's other selectors still match the element
        let sheet = parse("*,::before,::after { k: v }");
        assert!(matches(&d, p, &sheet.rules[0].selectors[0]));
    }

    #[test]
    fn computed_pseudo_resolves() {
        let d = doc("<body><div><p class=x>hi</p></div><p>other</p></body>");
        let p = d.find_deep(d.root, "p").unwrap();
        let sheet = parse(
            "p::before { content: 'x'; color: red }\
             p::after { content: 'y' }\
             div p.x::before { font-weight: bold }\
             p::marker { color: blue }",
        );
        let mut ctx = StyleCtx::new(&d, &[&sheet]);
        let before = ctx.computed_pseudo(p, PseudoElem::Before);
        assert_eq!(before.get("content").unwrap(), "'x'");
        assert_eq!(before.get("color").unwrap(), "red");
        assert_eq!(before.get("font-weight").unwrap(), "bold");
        let after = ctx.computed_pseudo(p, PseudoElem::After);
        assert_eq!(after.get("content").unwrap(), "'y'");
        let marker = ctx.computed_pseudo(p, PseudoElem::Marker);
        assert_eq!(marker.get("color").unwrap(), "blue");
        // the element's own computed style does NOT get pseudo decls
        let own = ctx.computed(p);
        assert!(own.get("content").is_none());
        assert_ne!(own.get("color").map(String::as_str), Some("red"));
    }

    #[test]
    fn pseudo_subject_requires_element_match() {
        let d = doc("<body><div><p></p></div><span><p></p></span></body>");
        let ps: Vec<_> = d
            .descendants(d.root)
            .filter(|&n| d.tag(n) == Some("p"))
            .collect();
        // `div p::before` — the element must match `div p`
        let sheet = parse("div p::before { k: hit } span p::before { k: miss }");
        let mut ctx = StyleCtx::new(&d, &[&sheet]);
        let a = ctx.computed_pseudo(ps[0], PseudoElem::Before);
        assert_eq!(a.get("k").unwrap(), "hit");
        let b = ctx.computed_pseudo(ps[1], PseudoElem::Before);
        assert_eq!(b.get("k").unwrap(), "miss");
        // bare `::before` = `*::before` — applies to any element
        let sheet = parse("::selection { background-color: #123456 }");
        let mut ctx = StyleCtx::new(&d, &[&sheet]);
        let s = ctx.computed_pseudo(ps[0], PseudoElem::Selection);
        assert_eq!(s.get("background-color").unwrap(), "#123456");
    }

    #[test]
    fn list_style_shorthand_splits() {
        let d = doc("<body><li></li></body>");
        let li = d.find_deep(d.root, "li").unwrap();
        let sheet = parse("li { list-style: inside square }");
        let c = computed(&d, li, &[&sheet]);
        assert_eq!(c.get("list-style-type").unwrap(), "square");
        assert_eq!(c.get("list-style-position").unwrap(), "inside");
        let sheet = parse("li { list-style: none }");
        let c = computed(&d, li, &[&sheet]);
        assert_eq!(c.get("list-style-type").unwrap(), "none");
        assert_eq!(c.get("list-style-image").unwrap(), "none");
        let sheet = parse("li { list-style: \"-\" inside }");
        let c = computed(&d, li, &[&sheet]);
        assert_eq!(c.get("list-style-type").unwrap(), "\"-\"");
        assert_eq!(c.get("list-style-position").unwrap(), "inside");
    }
}
