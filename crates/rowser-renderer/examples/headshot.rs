//! Headless site render: `headshot <url> <out.png> [scroll_px]`
//! fetch → parse → linked stylesheets → layout → paint → PNG.
//! Images are not loaded (text/layout regression check only).

use rowser_layout::Layouter;
use rowser_text::FontCache;
use std::collections::HashMap;
use std::io::{self, Read, Write};

fn fetch(url: &str) -> io::Result<Vec<u8>> {
    let parsed =
        rowser_net::Url::parse(url).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    match parsed.scheme.as_str() {
        "http" => {
            let mut tcp = std::net::TcpStream::connect((parsed.host.as_str(), parsed.port))?;
            tcp.set_read_timeout(Some(std::time::Duration::from_secs(20)))?;
            let r = rowser_net::request_over(&mut tcp, &parsed)?;
            Ok(r.body)
        }
        "https" => {
            let tcp = std::net::TcpStream::connect((parsed.host.as_str(), parsed.port))?;
            tcp.set_read_timeout(Some(std::time::Duration::from_secs(20)))?;
            let mut tls = rowser_tls::TlsStream::connect(&parsed.host, parsed.port, tcp)?;
            let _ = tls.verify_certificate();
            let r = rowser_net::request_over(&mut tls, &parsed)?;
            Ok(r.body)
        }
        "file" => {
            let p = rowser_net::percent_decode(url.trim_start_matches("file://"))
                .unwrap_or_else(|_| url.trim_start_matches("file://").to_string());
            std::fs::read(p)
        }
        s => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("scheme {s}"),
        )),
    }
}

fn resolve(base: &str, r: &str) -> Option<String> {
    let r = r.trim();
    if r.starts_with("data:") {
        return None;
    }
    if r.contains("://") {
        return Some(r.to_string());
    }
    let b = rowser_net::Url::parse(base).ok()?;
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

fn main() {
    let mut a = std::env::args().skip(1);
    let url = a.next().expect("usage: headshot <url> <out.png> [scroll]");
    let out = a.next().expect("out.png");
    let scroll: f32 = a.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let (w, h) = (1280.0f32, 800.0f32);

    let html = String::from_utf8_lossy(&fetch(&url).expect("fetch failed")).into_owned();
    let doc = rowser_html::parse(&html);

    // linked stylesheets (inline <style> is collected by layout itself)
    let mut sheets = Vec::new();
    for id in doc.descendants(doc.root) {
        if doc.tag(id) == Some("link")
            && doc
                .attr(id, "rel")
                .unwrap_or("")
                .to_lowercase()
                .contains("stylesheet")
        {
            if let Some(h) = doc.attr(id, "href").and_then(|h| resolve(&url, h)) {
                if let Ok(b) = fetch(&h) {
                    sheets.push(rowser_css::parse_vw(&String::from_utf8_lossy(&b), w));
                }
            }
        }
    }
    eprintln!("{} linked sheets", sheets.len());

    let mut fonts = FontCache::new();
    let (items, height, _anchors, _hits) =
        Layouter::layout_vh(&doc, &mut fonts, w, scroll, h, &sheets);
    eprintln!("{} items, doc height {height:.0}", items.len());
    let px = rowser_paint::paint(
        &items,
        &mut fonts,
        w as u32,
        h as u32,
        0xffffffff,
        &HashMap::new(),
    );
    write_png(&out, w as u32, h as u32, &px).expect("png write");
    eprintln!("wrote {out}");
    io::stderr().flush().ok();
}
