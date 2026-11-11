//! Benchmark: parse a saved real-world page and time `Layouter::layout`.
//! Prints a fingerprint of the display list as an output invariant.
//!
//!   cargo run --release -p rowser-layout --example layout_bench -- page.html [extra.css ...]

use rowser_layout::{DisplayItem, Layouter};
use rowser_text::FontCache;
use std::time::Instant;

fn fingerprint(items: &[DisplayItem]) -> u64 {
    // FNV-1a over a cheap serialization of each item.
    let mut h = 0xcbf29ce484222325u64;
    let mut mix = |b: &[u8]| {
        for &x in b {
            h ^= x as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    };
    for it in items {
        match it {
            DisplayItem::Text { x, baseline, text, font, size, color, underline, strike, href } => {
                mix(&[1]);
                mix(&x.to_bits().to_le_bytes());
                mix(&baseline.to_bits().to_le_bytes());
                mix(text.as_bytes());
                mix(&font.to_le_bytes());
                mix(&size.to_bits().to_le_bytes());
                mix(&color.to_le_bytes());
                mix(&[*underline as u8, *strike as u8]);
                mix(href.as_deref().unwrap_or("").as_bytes());
            }
            DisplayItem::FillRect { x, y, w, h, color } => {
                mix(&[2]);
                for v in [x, y, w, h] {
                    mix(&v.to_bits().to_le_bytes());
                }
                mix(&color.to_le_bytes());
            }
            DisplayItem::RoundedRect { x, y, w, h, radius, color } => {
                mix(&[3]);
                for v in [x, y, w, h, radius] {
                    mix(&v.to_bits().to_le_bytes());
                }
                mix(&color.to_le_bytes());
            }
            DisplayItem::Raster { x, y, w, h, pixels } => {
                mix(&[4]);
                for v in [x, y, w, h] {
                    mix(&v.to_bits().to_le_bytes());
                }
                mix(&(pixels.len() as u64).to_le_bytes());
                mix(&pixels[..pixels.len().min(64)]);
            }
            DisplayItem::Image { x, y, w, h, src } => {
                mix(&[5]);
                for v in [x, y, w, h] {
                    mix(&v.to_bits().to_le_bytes());
                }
                mix(src.as_bytes());
            }
            DisplayItem::BorderSide { x, y, w, h, side, style, color } => {
                mix(&[6]);
                for v in [x, y, w, h] {
                    mix(&v.to_bits().to_le_bytes());
                }
                mix(&[*side as u8, *style as u8]);
                mix(&color.to_le_bytes());
            }
            DisplayItem::RoundedRect2 { x, y, w, h, radii, color } => {
                mix(&[7]);
                for v in [x, y, w, h] {
                    mix(&v.to_bits().to_le_bytes());
                }
                for c in radii {
                    for v in c {
                        mix(&v.to_bits().to_le_bytes());
                    }
                }
                mix(&color.to_le_bytes());
            }
            DisplayItem::Shadow { x, y, w, h, dx, dy, blur, spread, radii, color, inset } => {
                mix(&[8]);
                for v in [x, y, w, h, dx, dy, blur, spread] {
                    mix(&v.to_bits().to_le_bytes());
                }
                for c in radii {
                    for v in c {
                        mix(&v.to_bits().to_le_bytes());
                    }
                }
                mix(&color.to_le_bytes());
                mix(&[*inset as u8]);
            }
            DisplayItem::PushClip { x, y, w, h, radii } => {
                mix(&[9]);
                for v in [x, y, w, h] {
                    mix(&v.to_bits().to_le_bytes());
                }
                for c in radii {
                    for v in c {
                        mix(&v.to_bits().to_le_bytes());
                    }
                }
            }
            DisplayItem::PopClip => mix(&[10]),
        }
    }
    h
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| "/tmp/wiki_dec.html".to_string());
    // remaining args: "N" = iteration count, otherwise external css file
    let mut iters = 3usize;
    let mut css_files = Vec::new();
    for a in args {
        if let Ok(n) = a.parse::<usize>() {
            iters = n;
        } else {
            css_files.push(a);
        }
    }
    let html = std::fs::read_to_string(&path).expect("read page");

    let t = Instant::now();
    let doc = rowser_html::parse(&html);
    println!("parse: {:?}  nodes={}", t.elapsed(), doc.nodes.len());

    let t = Instant::now();
    let doc_sheets = rowser_css::document_sheets(&doc);
    let mut external: Vec<rowser_css::StyleSheet> = Vec::new();
    for f in &css_files {
        let css = std::fs::read_to_string(f).expect("read css");
        external.push(rowser_css::parse(&css));
    }
    println!(
        "sheets: {:?}  inline={} ext={} rules={}+{}",
        t.elapsed(),
        doc_sheets.len(),
        external.len(),
        doc_sheets.iter().map(|s| s.rules.len()).sum::<usize>(),
        external.iter().map(|s| s.rules.len()).sum::<usize>(),
    );

    let mut fonts = FontCache::new();
    let mut best = std::time::Duration::MAX;
    for i in 0..iters {
        let t = Instant::now();
        let (items, h, anchors, _) =
            Layouter::layout_vh(&doc, &mut fonts, 1024.0, 0.0, 768.0, &external);
        let el = t.elapsed();
        best = best.min(el);
        println!(
            "layout[{i}]: {:?}  items={} height={h:.0} anchors={} fp={:016x}",
            el,
            items.len(),
            anchors.len(),
            fingerprint(&items)
        );
    }
    println!("best: {best:?}");
}
