//! @font-face loading: pick a usable src per declared face (local() or a
//! decodable url()), fetch bytes, and register them in the FontCache
//! under the declared family name.
//!
//! src selection: local() names try the installed set first (free — no
//! fetch); url()s are ranked decodable-first (truetype/opentype/woff
//! before woff2, which we can't unwrap without brotli) and fetched on the
//! shared pool in rounds — a failed fetch/decode falls through to the
//! face's next src.

use rowser_css::{FontFace, FontFaceStyle, FontSrc};
use rowser_text::FontCache;

/// Cap on web fonts processed per page — pathological sheets shouldn't
/// stall first paint on an unbounded font fetch.
const MAX_FONT_FACES: usize = 24;
const FONT_FETCH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// Register every declared face in `faces` under its family name.
/// `base` resolves relative url()s; results land in `fonts`.
pub fn register_font_faces(faces: &[FontFace], base: &str, fonts: &mut FontCache) {
    struct Pending<'a> {
        face: &'a FontFace,
        urls: Vec<String>,
        next: usize,
        weight: u16,
        italic: bool,
    }
    let mut pend: Vec<Pending> = Vec::new();
    for face in faces.iter().take(MAX_FONT_FACES) {
        if face.family.is_empty() {
            continue;
        }
        let weight = (face.weight.0 + face.weight.1) / 2;
        let italic = match face.style {
            FontFaceStyle::Normal => false,
            FontFaceStyle::Italic => true,
            FontFaceStyle::Oblique(a) => a != 0.0,
        };
        // src order: locals first (installed faces cost no fetch), then
        // urls ranked by what we can decode.
        let mut registered = false;
        let mut urls: Vec<(u8, String)> = Vec::new();
        for src in &face.src {
            match src {
                FontSrc::Local(name) => {
                    if !registered
                        && fonts
                            .register_local(
                                &face.family,
                                name,
                                weight,
                                italic,
                                face.unicode_range.clone(),
                            )
                            .is_some()
                    {
                        registered = true;
                        break; // first working local() wins
                    }
                }
                FontSrc::Url { url, format, .. } => {
                    if let Some(abs) = crate::resolve_url(base, url) {
                        let rank: u8 = match format.as_deref() {
                            None | Some("") => 0,
                            Some(
                                "truetype" | "opentype" | "woff" | "truetype-variations"
                                | "opentype-variations" | "woff-variations"
                                | "truetype-collection" | "opentype-collection"
                                | "woff-collection",
                            ) => 0,
                            Some("woff2") => 1,
                            Some(_) => 2,
                        };
                        urls.push((rank, abs));
                    }
                }
            }
        }
        if registered {
            continue;
        }
        urls.sort_by_key(|&(r, _)| r);
        let mut dedup: Vec<String> = Vec::new();
        for (_, u) in urls {
            if !dedup.contains(&u) {
                dedup.push(u);
            }
        }
        if !dedup.is_empty() {
            pend.push(Pending {
                face,
                urls: dedup,
                next: 0,
                weight,
                italic,
            });
        }
    }
    // Rounds: every pending face contributes its next untried url to one
    // pooled fetch; failures fall through to the next round's url.
    loop {
        let mut round: Vec<String> = Vec::new();
        let mut picks: Vec<(usize, String)> = Vec::new();
        for (i, p) in pend.iter_mut().enumerate() {
            if p.next < p.urls.len() {
                let u = p.urls[p.next].clone();
                p.next += 1;
                if !round.contains(&u) {
                    round.push(u.clone());
                }
                picks.push((i, u));
            }
        }
        if picks.is_empty() {
            break;
        }
        let bodies = crate::fetch_pool(&round, FONT_FETCH_DEADLINE);
        for (i, u) in picks {
            let Some(body) = bodies.get(&u) else { continue };
            let Some(sfnt) = decode_font_bytes(body) else { continue };
            let p = &pend[i];
            if fonts
                .register_bytes(
                    &p.face.family,
                    &sfnt,
                    p.weight,
                    p.italic,
                    p.face.unicode_range.clone(),
                )
                .is_some()
            {
                pend[i].next = usize::MAX; // registered — stop trying srcs
            }
        }
    }
}

/// Font file bytes → sfnt bytes. WOFF ("wOFF") wraps zlib-compressed
/// tables — unwrap them back into a bare sfnt. WOFF2 ("wOF2") needs
/// brotli, which we don't have. Anything else passes through for
/// `Font::parse` to judge (it rejects CFF/non-sfnt anyway).
fn decode_font_bytes(body: &[u8]) -> Option<Vec<u8>> {
    if body.starts_with(b"wOFF") {
        return unwrap_woff(body);
    }
    if body.starts_with(b"wOF2") {
        return None;
    }
    Some(body.to_vec())
}

/// WOFF (Web Open Font Format 1.0) → sfnt. The 44-byte header is followed
/// by 20-byte table records (tag, offset, compLength, origLength,
/// origChecksum); tables whose compLength < origLength are zlib streams.
/// Table data is re-laid 4-byte-aligned behind a fresh sfnt directory.
fn unwrap_woff(d: &[u8]) -> Option<Vec<u8>> {
    let u16b = |o: usize| -> Option<u16> {
        Some(u16::from_be_bytes([*d.get(o)?, *d.get(o + 1)?]))
    };
    let u32b = |o: usize| -> Option<u32> {
        Some(u32::from_be_bytes([
            *d.get(o)?,
            *d.get(o + 1)?,
            *d.get(o + 2)?,
            *d.get(o + 3)?,
        ]))
    };
    if d.len() < 44 {
        return None;
    }
    let flavor = u32b(4)?;
    let num = u16b(12)? as usize;
    if num == 0 || 44 + num * 20 > d.len() {
        return None;
    }
    struct T {
        tag: [u8; 4],
        sum: u32,
        data: Vec<u8>,
    }
    let mut tables: Vec<T> = Vec::with_capacity(num);
    for i in 0..num {
        let o = 44 + i * 20;
        let tag = [d[o], d[o + 1], d[o + 2], d[o + 3]];
        let off = u32b(o + 4)? as usize;
        let clen = u32b(o + 8)? as usize;
        let olen = u32b(o + 12)? as usize;
        let sum = u32b(o + 16)?;
        let raw = d.get(off..off.checked_add(clen)?)?;
        let data = if clen < olen {
            rowser_deflate::inflate_zlib(raw).ok()?
        } else {
            raw.to_vec()
        };
        if data.len() != olen {
            return None;
        }
        tables.push(T { tag, sum, data });
    }
    // sfnt header: flavor + numTables + the binary-search hint fields
    let (mut p2, mut sel) = (1usize, 0u32);
    while p2 * 2 <= num {
        p2 *= 2;
        sel += 1;
    }
    let search = (p2 * 16) as u32;
    let range_shift = num as u32 * 16 - search;
    let mut out = Vec::with_capacity(d.len());
    out.extend_from_slice(&flavor.to_be_bytes());
    out.extend_from_slice(&(num as u16).to_be_bytes());
    out.extend_from_slice(&(search as u16).to_be_bytes());
    out.extend_from_slice(&(sel as u16).to_be_bytes());
    out.extend_from_slice(&(range_shift as u16).to_be_bytes());
    let mut off = 12usize + num * 16;
    for t in &tables {
        out.extend_from_slice(&t.tag);
        out.extend_from_slice(&t.sum.to_be_bytes());
        out.extend_from_slice(&(off as u32).to_be_bytes());
        out.extend_from_slice(&(t.data.len() as u32).to_be_bytes());
        off += (t.data.len() + 3) & !3;
    }
    for t in &tables {
        out.extend_from_slice(&t.data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }
    Some(out)
}
