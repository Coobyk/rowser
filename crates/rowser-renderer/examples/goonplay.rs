//! Interactive probe for goonchess.com: fetch → scripts → layout →
//! simulate clicks through the live DomContext → verify the DOM moves.
//! Usage: goonplay [dump|click]

use std::io;

fn fetch(url: &str, session: &mut rowser_net::Session) -> io::Result<(Vec<u8>, Option<String>)> {
    if let Some(p) = url.strip_prefix("file://") {
        let p = rowser_net::percent_decode(p).unwrap_or_else(|_| p.to_string());
        return std::fs::read(if p.is_empty() { "/" } else { &p }).map(|b| (b, None));
    }
    if url.starts_with('/') || url.starts_with("./") || url.starts_with("../") {
        return std::fs::read(url).map(|b| (b, None));
    }
    let resp = session.fetch(url)?;
    let cs = resp.charset();
    Ok((resp.body, cs))
}

fn resolve(base: &str, href: &str) -> Option<String> {
    let b = rowser_net::Url::parse(base).ok()?;
    b.resolve(href).ok().map(|u| u.to_string())
}

/// Fetch + run all page scripts; returns the LIVE DomContext so we can
/// dispatch events afterwards (mirrors headshot's run_scripts).
fn scripts_ctx(
    doc: rowser_dom::Document,
    page_url: &str,
    w: f32,
    h: f32,
    session: &mut rowser_net::Session,
) -> rowser_js::dom_bridge::DomContext {
    struct Script {
        src: Option<String>,
        body: String,
    }
    let mut scripts = Vec::new();
    for id in doc.descendants(doc.root) {
        if doc.tag(id) != Some("script") {
            continue;
        }
        let ty = doc
            .attr(id, "type")
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if !matches!(
            ty.as_str(),
            "" | "text/javascript"
                | "application/javascript"
                | "module"
                | "text/ecmascript"
                | "application/ecmascript"
        ) || doc.attr(id, "nomodule").is_some()
        {
            continue;
        }
        let src = doc.attr(id, "src").map(|s| s.to_string());
        let body = if src.is_none() {
            doc.children(id)
                .iter()
                .filter_map(|&c| doc.text(c))
                .collect::<String>()
        } else {
            String::new()
        };
        scripts.push(Script { src, body });
    }
    let mut dc = rowser_js::dom_bridge::DomContext::with_viewport(
        doc,
        page_url,
        w as f64,
        h as f64,
    );
    for s in &scripts {
        let src = match &s.src {
            Some(src) => match resolve(page_url, src)
                .and_then(|u| fetch(&u, session).ok().map(|(b, _)| b))
            {
                Some(b) => String::from_utf8_lossy(&b).into_owned(),
                None => {
                    eprintln!("[js] failed to fetch {src}");
                    continue;
                }
            },
            None => s.body.clone(),
        };
        if let Err(e) = dc.eval(&src) {
            eprintln!("[js] {e}");
        }
    }
    dc.flush_raf();
    dc.run_timers();
    for e in &dc.errors {
        eprintln!("[js] {e}");
    }
    for line in dc.ctx.take_output() {
        eprintln!("[js console] {line}");
    }
    dc
}

fn attrs_of(d: &rowser_dom::Document, n: rowser_dom::NodeId) -> Vec<(String, String)> {
    match d.kind(n) {
        rowser_dom::NodeKind::Element { attrs, .. } => {
            attrs.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        }
        _ => Vec::new(),
    }
}

/// Recursively print elements matching the board-square heuristic.
fn dump_tree(d: &rowser_dom::Document, n: rowser_dom::NodeId, depth: usize, max: &mut usize) {
    if *max == 0 {
        return;
    }
    let kids = d.children(n);
    // Board: element with exactly 64 element children, or children whose
    // attrs name a square (data-square / aria-label / class w/ square).
    if kids.len() == 64
        && kids
            .iter()
            .filter(|&&c| d.tag(c).is_some())
            .count()
            == 64
    {
        *max -= 1;
        eprintln!("== 64-child element {} {:?} class={:?}", n, d.tag(n), d.attr(n, "class"));
        for (i, &c) in kids.iter().enumerate() {
            let attrs: Vec<String> = attrs_of(&d, c)
                .iter()
                .map(|(k, v)| format!("{k}={v:?}"))
                .collect();
            eprintln!("  [{i}] {} {:?}", d.tag(c).unwrap_or("?"), attrs);
            for &g in d.children(c).iter().take(4) {
                let ga: Vec<String> = attrs_of(&d, g)
                    .iter()
                    .map(|(k, v)| format!("{k}={:?}", &v[..v.len().min(80)]))
                    .collect();
                eprintln!("      {} {:?} text={:?}", d.tag(g).unwrap_or("?"), ga, d.text_content(g).chars().take(30).collect::<String>());
            }
        }
    }
    for &c in kids {
        dump_tree(d, c, depth + 1, max);
    }
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "dump".into());
    let url = "https://goonchess.com/";
    let (w, h) = (1280.0f32, 800.0f32);
    let mut session = rowser_net::Session::with_google_consent();
    let (body, cs) = fetch(url, &mut session).expect("fetch");
    let html = rowser_html::decode(&body, cs.as_deref());
    let doc = rowser_html::parse_scripting(&html);
    let mut dc = scripts_ctx(doc, url, w, h, &mut session);

    if mode == "dump" {
        let d = dc.snapshot_document();
        let mut n = 4;
        dump_tree(&d, d.root, 0, &mut n);
        // Also dump elements carrying square-ish attrs anywhere
        for id in d.descendants(d.root) {
            for (k, v) in attrs_of(&d, id) {
                if k.contains("square") || (k == "aria-label" && v.len() <= 3) {
                    eprintln!("attr-hit {} {:?} {k}={v:?} class={:?}", id, d.tag(id), d.attr(id, "class"));
                    break;
                }
            }
        }
        return;
    }

    // click mode: find the 64-button board, click e2 (idx 52) then e4
    // (idx 36), then verify the DOM changed (piece moved / move list).
    let doc = dc.snapshot_document();
    let board = doc
        .descendants(doc.root)
        .find(|&n| {
            let ks = doc.children(n);
            ks.len() == 64 && ks.iter().all(|&c| doc.tag(c) == Some("button"))
        })
        .expect("no 64-button board found");
    let squares: Vec<rowser_dom::NodeId> = doc.children(board).to_vec();
    eprintln!("board found: {board}, {} squares", squares.len());
    let before = piece_map(&doc, &squares);
    eprintln!("pieces before: {:?}", &before[..8.min(before.len())]);

    // e2 (white pawn) -> e4. Board index: rank 8-file a = 0, so
    // e2 = row 6 col 4 = 52, e4 = row 4 col 4 = 36.
    for (name, idx) in [("e2", 52usize), ("e4", 36usize)] {
        let nid = squares[idx];
        match dc.dispatch_event(nid, "click", (400.0, 400.0)) {
            Ok(prevented) => eprintln!("click {name} dispatched (defaultPrevented={:?})", !prevented),
            Err(e) => eprintln!("click {name} error"),
        }
        dc.flush_raf();
        dc.run_timers();
    }
    for e in &dc.errors {
        eprintln!("[js] {e}");
    }
    for line in dc.ctx.take_output() {
        eprintln!("[js console] {line}");
    }
    let after = dc.snapshot_document();
    let amap = piece_map(&after, &squares);
    eprintln!("pieces after:  {:?}", &amap[..8.min(amap.len())]);
    let moved: Vec<usize> = (0..64)
        .filter(|&i| before.get(i) != amap.get(i))
        .collect();
    eprintln!("changed squares: {moved:?}");
    // move list / status text search
    for id in after.descendants(after.root) {
        let t = after.text_content(id);
        let t = t.trim();
        if t.len() > 3 && t.len() < 200 && (t.contains("e4") || t.contains("e2") || t.contains("1.")) {
            if after.tag(id).is_some() && !matches!(after.tag(id), Some("html") | Some("body") | Some("main") | Some("div")) {
                continue;
            }
            eprintln!("  text-node cand: {:?} {:?}", after.tag(id), &t[..t.len().min(80)]);
        }
    }
}

/// Map each square index to a signature of its piece subtree (span/img
/// presence) so a move shows up as a changed square.
fn piece_map(d: &rowser_dom::Document, squares: &[rowser_dom::NodeId]) -> Vec<String> {
    squares
        .iter()
        .map(|&sq| {
            let mut sig = String::new();
            for n in d.descendants(sq) {
                if let Some(t) = d.tag(n) {
                    sig.push_str(&format!("{t}:{:?};", d.attr(n, "class").unwrap_or("")));
                }
                if let Some(t) = d.text(n) {
                    sig.push_str(t.trim());
                }
            }
            sig
        })
        .collect()
}
