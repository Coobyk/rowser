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
                    let pixels = test_pattern(width.max(1), height.max(1), "blank");
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
    url: String,
    title: String,
    viewport: (u32, u32),
    scroll: f32,
    content_h: f32,
    pixels: Vec<u8>,
    items: Vec<rowser_layout::DisplayItem>,
}

impl Page {
    fn new() -> Self {
        Self {
            doc: None,
            url: "about:blank".into(),
            title: String::new(),
            viewport: (800, 600),
            scroll: 0.0,
            content_h: 0.0,
            pixels: Vec::new(),
            items: Vec::new(),
        }
    }

    fn has_doc(&self) -> bool {
        self.doc.is_some()
    }

    fn load(&mut self, doc: rowser_dom::Document, url: String, title: String) {
        self.doc = Some(doc);
        self.url = url;
        self.title = title;
        self.scroll = 0.0;
    }

    fn max_scroll(&self) -> f32 {
        (self.content_h - self.viewport.1 as f32).max(0.0)
    }

    fn repaint(&mut self, fonts: &mut FontCache) {
        let Some(doc) = &self.doc else { return };
        let (items, height) =
            Layouter::layout(doc, fonts, self.viewport.0 as f32, self.scroll);
        self.content_h = height;
        self.items = items;
        self.pixels = rowser_paint::paint(&self.items,
            fonts,
            self.viewport.0,
            self.viewport.1,
            bgra(0xff, 0xff, 0xff),
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
        let path = percent_decode(rest);
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
    if let Some(rest) = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://")) {
        return (
            error_page(url, &format!("network stack not built yet — {rest}")),
            url.to_string(),
        );
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

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v as char);
                i += 3;
                continue;
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

fn error_page(url: &str, why: &str) -> String {
    format!(
        "<html><body><h1>Cannot display page</h1><p><b>{url}</b></p><p>{why}</p><hr><p><small>rowser error page</small></p></body></html>"
    )
}

/// Premultiplied-BGRA checkerboard for the blank/no-document state.
fn test_pattern(width: u32, height: u32, _seed: &str) -> Vec<u8> {
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
