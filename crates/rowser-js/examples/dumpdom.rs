fn main() {
    let doc =
        rowser_html::parse(r#"<!doctype html><html><body><div id="root"></div></body></html>"#);
    let mut dc = rowser_js::dom_bridge::DomContext::with_viewport(
        doc,
        "https://goonchess.com/",
        1920.0,
        1080.0,
    );
    let src = std::fs::read_to_string("/tmp/goon.js").unwrap();
    let _ = dc.eval(&src);
    dc.flush_raf();
    dc.run_timers();
    let d = dc.take_document();
    fn walk(d: &rowser_dom::Document, n: rowser_dom::NodeId) {
        if d.children(n).len() == 64 {
            println!(
                "FOUND board: style={:?}",
                d.attr(n, "style")
                    .map(|s| s[..s.len().min(300)].to_string())
            );
        }
        for &c in d.children(n) {
            walk(d, c)
        }
    }
    walk(&d, d.root);
}
