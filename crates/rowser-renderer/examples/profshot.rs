//! Pipeline profiler: fetch (or file) → parse → css → loop{layout, paint}.
//! Times each stage; loops the hot stages so optimizations are visible.
//! Usage: profshot <url-or-path> [iters]

use rowser_layout::Layouter;
use rowser_text::FontCache;
use std::collections::HashMap;
use std::io::{self, Write as _};
use std::time::Instant;

/// (body, charset-hint) — same logic as headshot's fetch.
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

fn resolve(base: &rowser_net::Url, href: &str) -> Option<String> {
    base.resolve(href).ok().map(|u| u.to_string())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let url = args.next().unwrap_or_else(|| "https://en.wikipedia.org".into());
    let iters: u32 = args.next().and_then(|a| a.parse().ok()).unwrap_or(10);

    let (w, h) = (1280.0f32, 800.0f32);

    let t = Instant::now();
    let mut session = rowser_net::Session::with_google_consent();
    let (body, charset) = fetch(&url, &mut session).expect("fetch");
    let html = rowser_html::decode(&body, charset.as_deref());
    eprintln!("fetch+decode: {:?}", t.elapsed());

    let t = Instant::now();
    let doc = rowser_html::parse(&html);
    eprintln!("html parse: {:?}", t.elapsed());

    let t = Instant::now();
    let mut sheets = Vec::new();
    if url.starts_with("http") || url.starts_with("file://") {
        if let Ok(base) = rowser_net::Url::parse(&url) {
            let mut hrefs = Vec::new();
            for id in doc.descendants(doc.root) {
                if doc.tag(id) == Some("link")
                    && doc
                        .attr(id, "rel")
                        .is_some_and(|r| r.to_ascii_lowercase().contains("stylesheet"))
                {
                    if let Some(href) = doc.attr(id, "href") {
                        hrefs.push(href.to_string());
                    }
                }
            }
            for href in hrefs {
                if let Some(u) = resolve(&base, &href) {
                    if let Ok((b, _)) = fetch(&u, &mut session) {
                        sheets.push(rowser_css::parse(&String::from_utf8_lossy(&b)));
                    }
                }
            }
        }
    }
    eprintln!(
        "linked css fetch+parse: {:?} ({} sheets)",
        t.elapsed(),
        sheets.len()
    );

    let mut fonts = FontCache::new();
    let t = Instant::now();
    let (items, height, _a, hits) = Layouter::layout_vh(&doc, &mut fonts, w, 0.0, h, &sheets);
    eprintln!(
        "first layout: {:?} ({} items, h={height:.0})",
        t.elapsed(),
        items.len()
    );

    // warm layout loop — measures steady-state cost like per-repaint
    let t = Instant::now();
    for _ in 0..iters {
        let (it, _, _, _) = Layouter::layout_vh(&doc, &mut fonts, w, 0.0, h, &sheets);
        std::hint::black_box(it);
    }
    eprintln!("layout x{iters}: {:?} ({:?}/iter)", t.elapsed(), t.elapsed() / iters);

    // isolate: UA-sheet-only layout (no site styles) separates selector
    // matching/cascade cost from box+text mechanics
    let no_sheets: Vec<rowser_css::StyleSheet> = Vec::new();
    let t = Instant::now();
    for _ in 0..iters {
        let (it, _, _, _) = Layouter::layout_vh(&doc, &mut fonts, w, 0.0, h, &no_sheets);
        std::hint::black_box(it);
    }
    eprintln!("layout-nocss x{iters}: {:?} ({:?}/iter)", t.elapsed(), t.elapsed() / iters);

    let t = Instant::now();
    for _ in 0..iters {
        let px = rowser_paint::paint_par(
            &items,
            &mut fonts,
            w as u32,
            h as u32,
            0xffffffff,
            &HashMap::new(),
        );
        std::hint::black_box(px);
    }
    eprintln!("paint x{iters}: {:?} ({:?}/iter)", t.elapsed(), t.elapsed() / iters);

    let t = Instant::now();
    let mut gpu_ok = false;
    for _ in 0..iters {
        let px = rowser_paint::gpu::paint_gpu(
            &items,
            &mut fonts,
            w as u32,
            h as u32,
            0xffffffff,
            &HashMap::new(),
        );
        gpu_ok |= px.is_some();
        std::hint::black_box(px);
    }
    eprintln!(
        "paint-gpu x{iters}: {:?} ({:?}/iter) active={gpu_ok}",
        t.elapsed(),
        t.elapsed() / iters
    );

    eprintln!("(items: {}, hits: {})", items.len(), hits.len());
    io::stderr().flush().ok();
}
