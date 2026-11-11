use rowser_layout::{Layouter, DisplayItem};
use rowser_text::FontCache;
fn main() {
    let doc = rowser_html::parse(r#"<!doctype html><html><body><div id="root"></div></body></html>"#);
    let mut dc = rowser_js::dom_bridge::DomContext::with_viewport(doc, "https://goonchess.com/", 1920.0, 1080.0);
    let src = std::fs::read_to_string("/tmp/goon.js").unwrap();
    let _ = dc.eval(&src);
    dc.flush_raf(); dc.run_timers();
    let d = dc.take_document();
    // find the board container (64 children, grid style)
    fn find<'a>(d: &'a rowser_dom::Document, n: rowser_dom::NodeId) -> Option<rowser_dom::NodeId> {
        if d.children(n).len() == 64 && d.attr(n, "style").is_some_and(|s| s.contains("grid-template")) {
            return Some(n);
        }
        d.children(n).iter().find_map(|&c| find(d, c))
    }
    let board = find(&d, d.root).expect("board not found");
    eprintln!("board style: {:?}", d.attr(board, "style"));
    eprintln!("board class: {:?}", d.attr(board, "class"));
    let css = std::fs::read_to_string("/tmp/goon.css").unwrap();
    let sheet = rowser_css::parse(&css);
    eprintln!("sheet rules: {}", sheet.rules.len());
    let grid_rule = sheet.rules.iter().any(|r| r.decls.iter().any(|dd| dd.prop=="display" && dd.value=="grid"));
    eprintln!("has display:grid rule: {grid_rule}");
    let bprops = rowser_css::computed(&d, board, &[&sheet]);
    eprintln!("board computed display={:?} gtc={:?} w={:?} pos={:?}", bprops.get("display"), bprops.get("grid-template-columns"), bprops.get("width"), bprops.get("position"));
    // find the lg:grid-cols container
    fn findgrid<'a>(d: &'a rowser_dom::Document, n: rowser_dom::NodeId, out: &mut Vec<rowser_dom::NodeId>) {
        if d.attr(n,"class").unwrap_or("").contains("grid-cols") { out.push(n) }
        for &ch in d.children(n) { findgrid(d,ch,out) }
    }
    let mut gs = Vec::new();
    findgrid(&d, d.root, &mut gs);
    for g in &gs {
        let p = rowser_css::computed(&d, *g, &[&sheet]);
        eprintln!("GCONT .{}: disp={:?} gtc={:?} w={:?}", d.attr(*g,"class").unwrap_or(""), p.get("display"), p.get("grid-template-columns"), p.get("width"));
    }
    // ancestors
    let mut cur = board;
    loop {
        let p = rowser_css::computed(&d, cur, &[&sheet]);
        eprintln!("anc {}.{}: disp={:?} w={:?} mw={:?} h={:?} ar={:?} pos={:?}", 
            d.tag(cur).unwrap_or("?"), d.attr(cur,"class").unwrap_or(""), p.get("display"), p.get("width"), p.get("max-width"), p.get("height"), p.get("aspect-ratio"), p.get("position"));
        match d.parent(cur) { Some(pp) if pp!=cur => cur=pp, _ => break }
    }
    let c0 = d.children(board)[0];
    let c1 = d.children(board)[1];
    let c9 = d.children(board)[9];
    // piece cells: dump markup recursively
    fn dump(d: &rowser_dom::Document, n: rowser_dom::NodeId, depth: usize, out: &mut String) {
        if depth > 5 { return }
        let pad = "  ".repeat(depth);
        match d.tag(n) {
            Some(t) => { out.push_str(&format!("{pad}<{t} class={:?} style={:?} src={:?}>\n", d.attr(n,"class"), d.attr(n,"style"), d.attr(n,"src"))); }
            None => { if let Some(t)=d.text(n) { out.push_str(&format!("{pad}text {:?}\n", &t[..t.len().min(40)])); } }
        }
        for &ch in d.children(n) { dump(d,ch,depth+1,out) }
    }
    for i in [0usize,4,48] {
        let c = d.children(board)[i];
        let mut s2 = String::new();
        dump(&d, c, 0, &mut s2);
        eprintln!("=== cell {i}:\n{s2}");
    }
    for (i,c) in [(0,c0),(1,c1),(9,c9)] {
        let p = rowser_css::computed(&d, c, &[&sheet]);
        eprintln!("cell{i}: tag={:?} class={:?} style={:?}", d.tag(c), d.attr(c,"class"), d.attr(c,"style"));
        eprintln!("   computed: disp={:?} w={:?} h={:?} bg={:?} bgimg={:?} pos={:?}", p.get("display"), p.get("width"), p.get("height"), p.get("background-color"), p.get("background"), p.get("position"));
        for &gc in d.children(c) {
            let gp = rowser_css::computed(&d, gc, &[&sheet]);
            eprintln!("   child {}.{}: disp={:?} bg={:?} style={:?} class={:?}", d.tag(gc).unwrap_or("?"), "", gp.get("display"), gp.get("background-color"), d.attr(gc,"style"), d.attr(gc,"class"));
        }
    }
    let mut fonts = FontCache::new();
    let (items, h, _, _) = Layouter::layout_vh(&d, &mut fonts, 1920.0, 0.0, 1080.0, &[sheet]);
    eprintln!("doc height {h}, {} display items", items.len());
    let mut kinds = std::collections::HashMap::new();
    for it in &items {
        let k = match it { DisplayItem::FillRect{..}=>"FillRect", DisplayItem::RoundedRect{..}=>"RoundedRect", DisplayItem::Text{..}=>"Text", DisplayItem::Image{..}=>"Image", _=>"other" };
        *kinds.entry(k).or_insert(0)+=1;
    }
    eprintln!("item kinds: {kinds:?}");
    for it in items.iter().take(25) { eprintln!("  {:?}", it); }
    for it in &items {
        let (x,y) = match it {
            DisplayItem::FillRect{x,y,..}|DisplayItem::RoundedRect{x,y,..}|DisplayItem::Raster{x,y,..}|DisplayItem::Image{x,y,..} => (*x,*y),
            DisplayItem::Text{x,baseline,..} => (*x,*baseline),
            _ => continue,
        };
        if x>=540.0&&x<=1060.0&&y>=70.0&&y<=600.0 {
            let desc = format!("{:?}", it);
            eprintln!("  BOARD-AREA {}", &desc[..desc.len().min(110)]);
        }
    }
    // collect candidate square cell rects (any kind of filled shape) in board x range
    let mut rects: Vec<(f32, f32, f32, f32)> = items.iter().filter_map(|i| match i {
        DisplayItem::FillRect { x, y, w, h, .. } => Some((*x, *y, *w, *h)),
        DisplayItem::RoundedRect { x, y, w, h, .. } => Some((*x, *y, *w, *h)),
        _ => None,
    }).collect();
    // dedupe identical rects (bg + borders share coords partially)
    rects.sort_by(|a, b| (a.1, a.0).partial_cmp(&(b.1, b.0)).unwrap());
    // cluster by ~64px cells: print first 20 distinct
    let mut shown = 0;
    let mut last: Option<(f32, f32)> = None;
    for r in &rects {
        if last.map_or(true, |(lx, ly)| (r.0 - lx).abs() > 1.0 || (r.1 - ly).abs() > 1.0) {
            if shown < 2000 { eprintln!("cell-ish rect {:.0},{:.0} {:.0}x{:.0}", r.0, r.1, r.2, r.3); }
            shown += 1;
            last = Some((r.0, r.1));
        }
    }
    eprintln!("distinct rect origins: {shown}");
}
