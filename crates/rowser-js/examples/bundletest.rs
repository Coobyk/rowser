fn main() {
    let doc = rowser_html::parse(r#"<!doctype html><html><body><div id="root"></div></body></html>"#);
    let mut dc = rowser_js::dom_bridge::DomContext::with_viewport(doc, "https://goonchess.com/", 1920.0, 1080.0);
    let mut src = std::fs::read_to_string("/tmp/goon.js").unwrap();
    src = src.replace("case F:s=10;break a;", "case F:console.log('MATCHED F'),s=10;break a;");
    src = src.replace("case P:s=9;break a;", "case P:console.log('MATCHED P'),s=9;break a;");
    src = src.replace("s=29,n=Error(i(130,", "console.log('BAD130'),s=29,n=Error(i(130,");
    let stmts = rowser_js::parser::parse(&src).unwrap();
    let g = dc.ctx.interp.global.clone();
    let _ = dc.ctx.interp.hoist_body(&stmts, &g);
    let mut threw = false;
    for (k, s) in stmts.iter().enumerate() {
        if let Err(v) = dc.ctx.interp.exec_stmt(s, &g) {
            println!("THROW stmt {k}: {}", dc.ctx.interp.throw_to_error(v));
            threw = true;
            break;
        }
    }
    if !threw {
        println!("all {} stmts ok", stmts.len());
    }
    dc.flush_raf();
    dc.run_timers();
    for l in dc.ctx.take_output() {
        println!("[console] {}", &l[..l.len().min(400)]);
    }
    for q in [
        "document.getElementById('root').children.length",
        "document.getElementById('root').innerHTML.slice(0,300)",
        "document.body.children.length",
    ] {
        match dc.eval(q) {
            Ok(v) => println!("{q} => {}", dc.ctx.interp.to_str(&v).unwrap_or_default()),
            Err(e) => println!("{q} => ERR {e}"),
        }
    }
    let d = dc.take_document();
    println!("dom nodes: {}", d.descendants(d.root).count());
}
