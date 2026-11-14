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
        if let Err(e) = dc.eval_bounded(&src, std::time::Duration::from_secs(30)) {
            eprintln!("[js] {e}");
        }
    }
    dc.pump_bounded(std::time::Duration::from_secs(20));
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
    let url = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "https://goonchess.com/".into());
    let (w, h) = (1280.0f32, 800.0f32);
    eprintln!("[goonplay] fetching {url}");
    let mut session = rowser_net::Session::with_google_consent();
    let (body, cs) = fetch(&url, &mut session).expect("fetch");
    eprintln!("[goonplay] fetched {} bytes", body.len());
    let html = rowser_html::decode(&body, cs.as_deref());
    let doc = rowser_html::parse_scripting(&html);
    eprintln!("[goonplay] parsed, running scripts");
    let mut dc = scripts_ctx(doc, &url, w, h, &mut session);
    eprintln!("[goonplay] scripts done");

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

    // Play real moves: click the source square to select, then click the
    // legal-move indicator inside the destination square, pump the
    // scheduler, and verify the DOM reflects the move.
    // Board index: rank 8-file a = 0 → e2=52, e4=36, e7=12, e5=28.
    for &(from, to, name) in [(52usize, 36usize, "e2-e4"), (12, 28, "e7-e5")].iter() {
        make_move(&mut dc, &squares, from, to, name);
        let d = dc.snapshot_document();
        eprint_occupancy(&d, &squares);
    }

    let mid = dc.snapshot_document();
    let mmap = piece_map(&mid, &squares);
    let moved: Vec<usize> = (0..64).filter(|&i| before.get(i) != mmap.get(i)).collect();
    eprintln!("[goonplay] squares changed vs start: {moved:?}");
    eprintln!("[goonplay] collecting snapshot");
    for e in &dc.errors {
        eprintln!("[js] {e}");
    }
    // Where was the interpreter when the budget tripped?
    if let Some(site) = &dc.ctx.interp.interrupt_site {
        eprintln!("interrupt site: {site}");
    }
    let tr: Vec<String> = dc.ctx.interp.call_trace.iter().cloned().collect();
    eprintln!("call_trace ({}):", tr.len());
    // Find the repeating cycle: detect the shortest period in the tail.
    if tr.len() > 100 {
        let tail = &tr[tr.len() - 100..];
        for period in 1..=50usize {
            if tail.windows(period).all(|w| w == &tail[..period]) {
                eprintln!("repeating cycle of {period}:");
                for t in &tail[..period] {
                    eprintln!("    | {t}");
                }
                break;
            }
        }
    }
    for t in tr.iter().take(30) {
        eprintln!("    {t}");
    }
    eprintln!("    ...");
    for t in tr.iter().skip(tr.len().saturating_sub(20)) {
        eprintln!("    {t}");
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
    eprint_occupancy(&after, &squares);
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

/// Click `from` to select, then click the legal-move dot inside `to`,
/// then pump the scheduler in 1s slices until queues drain (max ~8s).
fn make_move(
    dc: &mut rowser_js::dom_bridge::DomContext,
    squares: &[rowser_dom::NodeId],
    from: usize,
    to: usize,
    name: &str,
) {
    match dc.dispatch_event_pumped(squares[from], "click", (400.0, 400.0), std::time::Duration::from_secs(10)) {
        Ok(p) => eprintln!("[move {name}] select click ok (defaultPrevented={p})"),
        Err(e) => {
            eprintln!("[move {name}] select click err: {}", dc.ctx.interp.inspect(&e, 0));
            return;
        }
    }
    // Legal-move indicator: the dot (or capture ring) child on `to`.
    let mid = dc.snapshot_document();
    let dot = mid
        .descendants(squares[to])
        .find(|&n| {
            mid.attr(n, "class")
                .map(|c| c.contains("rounded-full") || c.contains("z-20"))
                .unwrap_or(false)
        });
    let target = match dot {
        Some(d) => d,
        None => {
            eprintln!("[move {name}] no legal-move dot on {to} — clicking square directly");
            squares[to]
        }
    };
    match dc.dispatch_event_pumped(target, "click", (400.0, 400.0), std::time::Duration::from_secs(10)) {
        Ok(p) => eprintln!("[move {name}] dest click ok (defaultPrevented={p})"),
        Err(e) => {
            eprintln!("[move {name}] dest click err: {}", dc.ctx.interp.inspect(&e, 0));
            return;
        }
    }
    for slice in 0..8 {
        let c = dc.pending_counts();
        if c.0 == 0 && c.1 == 0 && c.2 == 0 && c.3 == 0 {
            break;
        }
        dc.pump_bounded(std::time::Duration::from_secs(1));
        for e in dc.errors.drain(..) {
            eprintln!("    [js] {e}");
        }
        eprintln!("  pump {slice}: mt={} timers={} ports={} raf={} steps={}", c.0, c.1, c.2, c.3, c.4);
    }
    eprintln!("[move {name}] done");
}

/// Print an 8x8 occupancy map of piece img srcs (wP/bK/... or `.`).
fn eprint_occupancy(d: &rowser_dom::Document, squares: &[rowser_dom::NodeId]) {
    for r in 0..8 {
        let mut row = String::new();
        for c in 0..8 {
            let i = r * 8 + c;
            let mut mark = ".  ".to_string();
            for n in d.descendants(squares[i]) {
                if d.tag(n) == Some("img") {
                    if let Some(src) = d.attr(n, "src") {
                        if let Some(name) =
                            src.rsplit('/').next().and_then(|s| s.strip_suffix(".svg"))
                        {
                            mark = format!("{name:3}");
                        }
                    }
                }
            }
            row.push_str(&mark);
        }
        eprintln!("  r{r}: {row}");
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
