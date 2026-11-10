//! rowser-renderer: the per-tab child process.
//!
//! Speaks `rowser-ipc` on stdin/stdout. Loads documents (file://, built-in
//! rowser:// pages, error pages for everything else), runs them through the
//! engine pipeline — parse → layout → paint — and ships BGRA frames.

use rowser_ipc::{read_frame, write_frame, ToBrowser, ToRenderer};
use rowser_layout::{bgra, Layouter};
use rowser_text::FontCache;
use std::io::{self, Write};

const WELCOME: &str = include_str!("../../../assets/welcome.html");

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();

    let mut fonts = FontCache::new();
    let mut page = Page::new();

    loop {
        let payload = match read_frame(&mut input) {
            Ok(p) => p,
            Err(_) => break, // parent hung up
        };
        let msg = match ToRenderer::decode(&payload) {
            Ok(m) => m,
            Err(e) => {
                let _ = send(&mut output, &ToBrowser::Log(format!("bad message: {e}")));
                continue;
            }
        };
        match msg {
            ToRenderer::Ping(n) => {
                let _ = send(&mut output, &ToBrowser::Pong(n));
            }
            ToRenderer::Navigate(url) => {
                let _ = send(&mut output, &ToBrowser::LoadStarted);
                let (html, effective) = load_url(&url);
                let doc = rowser_html::parse(&html);
                let title = doc
                    .find_deep(doc.root, "title")
                    .map(|t| doc.text_content(t))
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| effective.clone());
                page.load(doc, effective, title.clone());
                page.repaint(&mut fonts);
                let _ = send(&mut output, &ToBrowser::Title(title));
                let _ = send(&mut output, &frame_msg(&page));
                let _ = send(&mut output, &ToBrowser::LoadDone);
            }
            ToRenderer::ResizeViewport { width, height } => {
                page.viewport = (width.max(1), height.max(1));
                if page.has_doc() {
                    page.repaint(&mut fonts);
                    let _ = send(&mut output, &frame_msg(&page));
                } else {
                    let pixels = test_pattern(width.max(1), height.max(1));
                    let _ = send(
                        &mut output,
                        &ToBrowser::Frame {
                            width: width.max(1),
                            height: height.max(1),
                            pixels,
                        },
                    );
                }
            }
            ToRenderer::Click { x, y } => {
                // hit-test link regions in document space
                let mut found = None;
                for it in &page.items {
                    if let rowser_layout::DisplayItem::Text {
                        x: tx,
                        baseline,
                        text,
                        font,
                        size,
                        href: Some(href),
                        ..
                    } = it
                    {
                        let _ = (tx, font);
                        let w = fonts
                            .font(*font)
                            .map(|f| f.measure(text, *size))
                            .unwrap_or(0.0);
                        // rough hit box: from x to x+width, baseline-ascent to baseline+descent
                        if x >= *tx && x <= *tx + w && y >= baseline - size && y <= *baseline + size * 0.4 {
                            found = Some(href.clone());
                            break;
                        }
                    }
                }
                if let Some(url) = found {
                    let _ = send(&mut output, &ToBrowser::NavigationRequest(url));
                }
            }
            ToRenderer::Scroll { dy } => {
                if page.has_doc() {
                    page.scroll = (page.scroll + dy).clamp(0.0, page.max_scroll());
                    page.repaint(&mut fonts);
                    let _ = send(&mut output, &frame_msg(&page));
                }
            }
            ToRenderer::DomDump => {
                if let Some(doc) = &page.doc {
                    let _ = send(&mut output, &ToBrowser::DomDump(dom_dump(doc)));
                }
            }
            ToRenderer::Shutdown => break,
        }
    }
}

fn send(w: &mut impl Write, msg: &ToBrowser) -> io::Result<()> {
    write_frame(w, &msg.encode())
}

struct Page {
    doc: Option<rowser_dom::Document>,
    viewport: (u32, u32),
    scroll: f32,
    content_h: f32,
    url_base: String,
    pixels: Vec<u8>,
    items: Vec<rowser_layout::DisplayItem>,
    images: std::collections::HashMap<String, rowser_image::Image>,
    sheets: Vec<rowser_css::StyleSheet>,
}

impl Page {
    fn new() -> Self {
        Self {
            doc: None,
            viewport: (800, 600),
            scroll: 0.0,
            content_h: 0.0,
            url_base: "about:blank".into(),
            pixels: Vec::new(),
            items: Vec::new(),
            images: std::collections::HashMap::new(),
            sheets: Vec::new(),
        }
    }

    fn has_doc(&self) -> bool {
        self.doc.is_some()
    }

    fn load(&mut self, doc: rowser_dom::Document, url: String, title: String) {
        self.doc = Some(doc);
        self.url_base = url;
        self.scroll = 0.0;
        self.images.clear();
        self.sheets.clear();
        self.load_linked_css();
    }

    /// Fetch <link rel="stylesheet" href=...> documents into style sheets.
    fn load_linked_css(&mut self) {
        let Some(doc) = &self.doc else { return };
        let mut hrefs = Vec::new();
        for id in doc.descendants(doc.root) {
            if doc.tag(id) == Some("link") {
                let rel = doc.attr(id, "rel").unwrap_or("").to_lowercase();
                if rel.contains("stylesheet") {
                    if let Some(h) = doc.attr(id, "href") {
                        hrefs.push(h.to_string());
                    }
                }
            }
        }
        for h in hrefs {
            if let Some(text) = fetch_text(&self.url_base, &h) {
                self.sheets.push(rowser_css::parse(&text));
            }
        }
    }

    /// Fetch + decode every <img src> referenced by the display list.
    fn load_images(&mut self) {
        let srcs: Vec<String> = self
            .items
            .iter()
            .filter_map(|it| {
                if let rowser_layout::DisplayItem::Image { src, .. } = it {
                    Some(src.clone())
                } else {
                    None
                }
            })
            .collect();
        for src in srcs {
            if self.images.contains_key(&src) {
                continue;
            }
            if let Some(bytes) = fetch_bytes(&self.url_base, &src) {
                if let Ok(img) = rowser_image::decode(&bytes) {
                    self.images.insert(src, img);
                }
            }
        }
    }

    fn max_scroll(&self) -> f32 {
        (self.content_h - self.viewport.1 as f32).max(0.0)
    }

    fn repaint(&mut self, fonts: &mut FontCache) {
        let Some(doc) = &self.doc else { return };
        let (items, height) =
            Layouter::layout(doc, fonts, self.viewport.0 as f32, self.scroll, &self.sheets);
        self.content_h = height;
        self.items = items;
        self.load_images();
        self.pixels = rowser_paint::paint(
            &self.items,
            fonts,
            self.viewport.0,
            self.viewport.1,
            bgra(0xff, 0xff, 0xff),
            &self.images,
        );
    }
}

fn frame_msg(page: &Page) -> ToBrowser {
    ToBrowser::Frame {
        width: page.viewport.0,
        height: page.viewport.1,
        pixels: page.pixels.clone(),
    }
}

/// Resolve a URL to HTML text + the effective URL (after fallback).
fn load_url(url: &str) -> (String, String) {
    if let Some(rest) = url.strip_prefix("file://") {
        let path = rowser_net::percent_decode(rest).unwrap_or_else(|_| rest.to_string());
        return match std::fs::read_to_string(&path) {
            Ok(html) => (html, url.to_string()),
            Err(e) => (error_page(url, &format!("cannot read {path}: {e}")), url.to_string()),
        };
    }
    if url.starts_with("rowser://") {
        let name = url.trim_start_matches("rowser://").trim_end_matches('/');
        if name == "bookmarks" || name == "history" {
            return (list_page(name), url.to_string());
        }
        return (WELCOME.to_string(), url.to_string());
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return match web_fetch(url) {
            Ok(res) => (
                String::from_utf8_lossy(&res.body).into_owned(),
                res.url.to_string(),
            ),
            Err(e) => (error_page(url, &format!("network error: {e}")), url.to_string()),
        };
    }
    if url == "about:blank" {
        return ("<html><body></body></html>".into(), url.into());
    }
    // Bare path? try as file
    if std::path::Path::new(url).exists() {
        if let Ok(html) = std::fs::read_to_string(url) {
            return (html, format!("file://{url}"));
        }
    }
    (error_page(url, "unrecognized URL"), url.to_string())
}

fn error_page(url: &str, why: &str) -> String {
    format!(
        "<html><body><h1>Cannot display page</h1><p><b>{url}</b></p><p>{why}</p><hr><p><small>rowser error page</small></p></body></html>"
    )
}

/// Premultiplied-BGRA checkerboard for the blank/no-document state.
fn test_pattern(width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut px = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let v = if ((x / 32) + (y / 32)) % 2 == 0 { 40u8 } else { 48u8 };
            px[i] = v;
            px[i + 1] = v;
            px[i + 2] = v + 8;
            px[i + 3] = 255;
        }
    }
    px
}


fn dom_dump(doc: &rowser_dom::Document) -> String {
    let mut s = String::new();
    dump_node(doc, doc.root, 0, &mut s);
    s
}

fn dump_node(doc: &rowser_dom::Document, id: rowser_dom::NodeId, d: usize, out: &mut String) {
    let pad = "  ".repeat(d.min(24));
    match doc.kind(id) {
        rowser_dom::NodeKind::Document => {}
        rowser_dom::NodeKind::Comment(_) => {}
        rowser_dom::NodeKind::Text(t) => {
            let t = t.trim();
            if !t.is_empty() {
                out.push_str(&format!("{pad}\"{}\"\n", &t[..t.len().min(60)]));
            }
        }
        rowser_dom::NodeKind::Element { tag, attrs } => {
            let a: String = attrs
                .iter()
                .map(|(k, v)| format!(" {k}=\"{}\"", &v[..v.len().min(40)]))
                .collect();
            out.push_str(&format!("{pad}<{tag}{a}>\n"));
        }
    }
    for &c in doc.children(id) {
        dump_node(doc, c, d + 1, out);
    }
}

/// rowser://bookmarks and rowser://history — built from ~/.config/rowser files.
fn list_page(name: &str) -> String {
    let file = format!("{}/.config/rowser/{name}.txt", std::env::var("HOME").unwrap_or_default());
    let mut items = String::new();
    if let Ok(text) = std::fs::read_to_string(&file) {
        for line in text.lines().rev() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            items.push_str(&format!("<li><a href=\"{}\">{}</a></li>", line, line));
        }
    }
    if items.is_empty() {
        items = "<li><i>nothing yet</i></li>".into();
    }
    format!("<html><body><h1>rowser:{name}</h1><ul>{items}</ul></body></html>")
}


/// Fetch a resource relative to a page URL — http(s), file://, or plain path.
fn fetch_bytes(base_url: &str, resource: &str) -> Option<Vec<u8>> {
    let url = resolve_url(base_url, resource)?;
    if let Some(path) = url.strip_prefix("file://") {
        let path = rowser_net::percent_decode(path).unwrap_or_else(|_| path.to_string());
        return std::fs::read(path).ok();
    }
    if url.starts_with("http") {
        return rowser_net::fetch(&url).ok().map(|r| r.body);
    }
    std::fs::read(&url).ok()
}

fn fetch_text(base_url: &str, resource: &str) -> Option<String> {
    fetch_bytes(base_url, resource).map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// Resolve `resource` against `base` (absolute URLs pass through).
fn resolve_url(base: &str, resource: &str) -> Option<String> {
    let r = resource.trim();
    if r.starts_with("data:") {
        return None; // no data: URLs yet
    }
    if r.contains("://") || r.starts_with("file:") {
        return Some(r.to_string());
    }
    let Ok(base_u) = rowser_net::Url::parse(base) else {
        // base was a local path → join directories
        if r.starts_with('/') {
            return Some(format!("file://{r}"));
        }
        let dir = std::path::Path::new(base)
            .parent()
            .unwrap_or(std::path::Path::new("/"));
        return Some(format!("file://{}", dir.join(r).display()));
    };
    let resolved = base_u.resolve(r).ok()?;
    Some(resolved.to_string())
}


/// http/https fetch with cross-scheme redirect handling.
fn web_fetch(url: &str) -> io::Result<rowser_net::Response> {
    use std::net::TcpStream;
    let mut parsed = rowser_net::Url::parse(url)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    for _ in 0..10 {
        let resp = match parsed.scheme.as_str() {
            "http" => {
                let mut tcp = TcpStream::connect((parsed.host.as_str(), parsed.port))?;
                rowser_net::request_over(&mut tcp, &parsed)?
            }
            "https" => {
                let tcp = TcpStream::connect((parsed.host.as_str(), parsed.port))?;
                let mut tls =
                    rowser_tls::TlsStream::connect(&parsed.host, parsed.port, tcp)?;
                if let Err(e) = tls.verify_certificate() {
                    return Ok(rowser_net::Response {
                        url: parsed.to_string(),
                        status: 0,
                        headers: vec![],
                        body: format!(
                            "<html><body><h1>Certificate error</h1><p>{e}</p></body></html>"
                        )
                        .into_bytes(),
                    });
                }
                rowser_net::request_over(&mut tls, &parsed)?
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("scheme {other}"),
                ))
            }
        };
        let is_redir = matches!(resp.status, 301 | 302 | 303 | 307 | 308);
        if !is_redir {
            return Ok(resp);
        }
        let loc = resp
            .header("location")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no Location"))?;
        parsed = parsed
            .resolve(loc.trim())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "too many redirects",
    ))
}
