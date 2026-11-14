//! Headless site render: `headshot <url> <out.png> [scroll_px]`
//! fetch → parse → linked stylesheets → layout → paint → PNG.
//! Images are not loaded (text/layout regression check only).

use rowser_layout::Layouter;
use rowser_text::FontCache;
use std::collections::HashMap;
use std::io::{self, Write};

/// (body, charset-hint) — the hint comes from the Content-Type header.
fn fetch(url: &str, session: &mut rowser_net::Session) -> io::Result<(Vec<u8>, Option<String>)> {
    // file:// (and bare paths) never reach Url::parse — `file:///x` has
    // an empty authority, which the URL parser rejects.
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

/// Fetch + splice `@import`ed sheets ahead of `sheet` (depth-capped).
fn expand_imports(
    sheet: rowser_css::StyleSheet,
    base: &str,
    vw: f32,
    out: &mut Vec<rowser_css::StyleSheet>,
    depth: u8,
    session: &mut rowser_net::Session,
) {
    if depth < 4 {
        for imp in &sheet.imports {
            if !rowser_css::media_query(&imp.media, vw) {
                continue;
            }
            let Some(u) = resolve(base, &imp.url) else {
                continue;
            };
            if let Ok((b, cs)) = fetch(&u, session) {
                let sh = rowser_css::parse_vw(
                    &rowser_html::decode(&b, cs.as_deref()),
                    vw,
                );
                expand_imports(sh, &u, vw, out, depth + 1, session);
            }
        }
    }
    out.push(sheet);
}

fn resolve(base: &str, r: &str) -> Option<String> {
    let r = r.trim();
    if r.starts_with("data:") {
        return None;
    }
    if r.contains("://") {
        return Some(r.to_string());
    }
    let Ok(b) = rowser_net::Url::parse(base) else {
        // file:// base — join against the document's directory
        let dir = std::path::Path::new(base.strip_prefix("file://").unwrap_or(base))
            .parent()
            .unwrap_or(std::path::Path::new("/"))
            .to_path_buf();
        let p = if r.starts_with('/') {
            std::path::PathBuf::from(r)
        } else {
            dir.join(r)
        };
        return Some(format!("file://{}", p.display()));
    };
    b.resolve(r).ok().map(|u| u.to_string())
}

// --- minimal PNG encoder (8-bit RGBA, zlib store blocks) ---

fn crc32(b: &[u8]) -> u32 {
    let mut c = 0xffffffffu32;
    for &x in b {
        c ^= x as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xedb88320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    !c
}

fn chunk(ty: &[u8; 4], data: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(ty);
    out.extend_from_slice(data);
    let mut crc = ty.to_vec();
    crc.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc).to_be_bytes());
}

fn write_png(path: &str, w: u32, h: u32, bgra: &[u8]) -> io::Result<()> {
    // raw scanlines: filter 0 + RGBA bytes
    let mut raw = Vec::with_capacity((w * h * 4 + h) as usize);
    for y in 0..h {
        raw.push(0u8);
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            raw.extend_from_slice(&[bgra[i + 2], bgra[i + 1], bgra[i], bgra[i + 3]]);
        }
    }
    // zlib store blocks
    let mut z = vec![0x78, 0x01];
    let mut pos = 0;
    while pos < raw.len() {
        let n = (raw.len() - pos).min(65535);
        let last = pos + n == raw.len();
        z.push(if last { 1 } else { 0 });
        z.extend_from_slice(&(n as u16).to_le_bytes());
        z.extend_from_slice(&(!(n as u16)).to_le_bytes());
        z.extend_from_slice(&raw[pos..pos + n]);
        pos += n;
    }
    let adler = {
        let (mut a, mut b) = (1u32, 0u32);
        for &x in &raw {
            a = (a + x as u32) % 65521;
            b = (b + a) % 65521;
        }
        (b << 16) | a
    };
    z.extend_from_slice(&adler.to_be_bytes());

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(b"IHDR", &ihdr, &mut png);
    chunk(b"IDAT", &z, &mut png);
    chunk(b"IEND", &[], &mut png);
    std::fs::write(path, &png)
}

/// Collect `<script>` elements in document order, fetch external bodies,
/// run everything in a DomContext and return the mutated Document (plus
/// logs on stderr). Mirror of the renderer's run_page_scripts, minus the
/// live event loop — screenshots only need the post-load snapshot.
fn run_scripts(
    doc: rowser_dom::Document,
    page_url: &str,
    w: f32,
    h: f32,
    session: &mut rowser_net::Session,
) -> rowser_dom::Document {
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
    if scripts.is_empty() {
        return doc;
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
    dc.snapshot_document()
}

fn main() {
    let mut a = std::env::args().skip(1);
    let url = a.next().expect("usage: headshot <url> <out.png> [scroll]");
    let out = a.next().expect("out.png");
    let scroll: f32 = a.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let (w, h) = (1280.0f32, 800.0f32);

    let mut session = rowser_net::Session::with_google_consent();
    let (body, cs) = fetch(&url, &mut session).expect("fetch failed");
    let html = rowser_html::decode(&body, cs.as_deref());
    if let Ok(p) = std::env::var("ROWSR_DUMP_HTML") {
        std::fs::write(p, &html).ok();
    }
    let js_on = std::env::var("ROWSER_JS").as_deref() != Ok("0");
    let doc = if js_on {
        rowser_html::parse_scripting(&html)
    } else {
        rowser_html::parse(&html)
    };

    // linked stylesheets (inline <style> is collected by layout itself) —
    // fetched in parallel; results come back in DOM order
    let sheet_urls: Vec<String> = doc
        .descendants(doc.root)
        .filter(|&id| {
            doc.tag(id) == Some("link")
                && doc
                    .attr(id, "rel")
                    .unwrap_or("")
                    .to_lowercase()
                    .contains("stylesheet")
        })
        .filter_map(|id| doc.attr(id, "href").and_then(|h| resolve(&url, h)))
        .collect();
    let mut sheets = Vec::new();
    for (h, res) in sheet_urls
        .iter()
        .zip(session.fetch_all(&sheet_urls))
    {
        if let Ok(resp) = res {
            let sh = rowser_css::parse_vw(
                &rowser_html::decode(&resp.body, resp.charset().as_deref()),
                w,
            );
            expand_imports(sh, h, w, &mut sheets, 0, &mut session);
        }
    }
    eprintln!("{} linked sheets", sheets.len());

    // page scripts against the parsed DOM (SPAs build their tree in JS)
    let doc = if js_on {
        run_scripts(doc, &url, w, h, &mut session)
    } else {
        doc
    };

    let mut fonts = FontCache::new();
    let (items, height, _anchors, _hits) =
        Layouter::layout_vh(&doc, &mut fonts, w, scroll, h, &sheets);
    eprintln!("{} items, doc height {height:.0}", items.len());
    if std::env::var("ROWSR_DUMP_ITEMS").is_ok() {
        for it in &items {
            eprintln!("  {it:?}");
        }
    }
    // Fetch + decode every image the display list references (like the
    // renderer's background image pipeline) — fetched in parallel.
    let mut srcs: Vec<String> = Vec::new();
    let mut img_urls: Vec<String> = Vec::new();
    for it in &items {
        let src = match it {
            rowser_layout::DisplayItem::Image { src, .. }
            | rowser_layout::DisplayItem::BgImage { src, .. } => src,
            _ => continue,
        };
        if srcs.contains(src) {
            continue;
        }
        if let Some(u) = resolve(&url, src) {
            srcs.push(src.clone());
            img_urls.push(u);
        }
    }
    let bodies = session.fetch_all(&img_urls);
    let mut images: HashMap<String, rowser_image::Image> = HashMap::new();
    for (src, res) in srcs.iter().zip(bodies) {
        let Ok(resp) = res else { continue };
        let body = resp.body;
        let img = rowser_image::decode(&body).ok().or_else(|| {
            let text = std::str::from_utf8(&body).ok()?;
            if !text[..text.len().min(512)].contains("<svg") {
                return None;
            }
            let doc = rowser_html::parse(text);
            let svg = doc.find_deep(doc.root, "svg")?;
            let dim = |name: &str| -> Option<f32> {
                doc.attr(svg, name)
                    .and_then(|v| v.trim_end_matches("px").parse().ok())
            };
            let (iw, ih) = match (dim("width"), dim("height")) {
                (Some(a), Some(b)) => (a, b),
                _ => doc
                    .attr(svg, "viewBox")
                    .or_else(|| doc.attr(svg, "viewbox"))
                    .and_then(|v| {
                        let n: Vec<f32> = v
                            .split(|c: char| c.is_whitespace() || c == ',')
                            .filter_map(|t| t.parse().ok())
                            .collect();
                        (n.len() >= 4).then(|| (n[2], n[3]))
                    })?,
            };
            let (rw, rh) = ((iw * 4.0) as u32, (ih * 4.0) as u32);
            Some(rowser_image::Image {
                width: rw,
                height: rh,
                pixels: rowser_layout::svg::render(&doc, svg, rw, rh, 0xff000000),
                density: 4.0,
            })
        });
        if let Some(img) = img {
            images.insert(src.clone(), img);
        }
    }
    eprintln!("{} images", images.len());

    let px = rowser_paint::gpu::paint_auto(
        &items,
        &mut fonts,
        w as u32,
        h as u32,
        0xffffffff,
        &images,
    );
    write_png(&out, w as u32, h as u32, &px).expect("png write");
    eprintln!("wrote {out}");
    io::stderr().flush().ok();
}
