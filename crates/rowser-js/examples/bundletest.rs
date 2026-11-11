fn main() {
    let doc = rowser_html::parse(r#"<!doctype html><html><body><div id="root"></div></body></html>"#);
    let mut dc = rowser_js::dom_bridge::DomContext::with_viewport(doc, "https://goonchess.com/", 1920.0, 1080.0);
    let src = std::fs::read_to_string("/tmp/goon.js").unwrap();
    let stmts = rowser_js::parser::parse(&src).unwrap();
    let g = dc.ctx.interp.global.clone();
    let _ = dc.ctx.interp.hoist_body(&stmts, &g);
    for (k, s) in stmts.iter().enumerate() {
        match dc.ctx.interp.exec_stmt(s, &g) {
            Ok(_) => {}
            Err(v) => {
                println!("THROW stmt {k}: {}", dc.ctx.interp.throw_to_error(v));
                for q in ["typeof Symbol","typeof Symbol.for","typeof o","typeof Symbol.for('x')","Symbol.for('x')===Symbol.for('x')"] {
                    let r = dc.ctx.interp.run(q);
                    match r { Ok(v)=>println!("  {q} => {}", dc.ctx.interp.to_str(&v).unwrap_or_default()), Err(e)=>println!("  {q} => {e}") }
                }
                for t in dc.ctx.interp.call_trace.iter().rev().take(6) { println!("  call: {t}"); }
                return;
            }
        }
    }
}
