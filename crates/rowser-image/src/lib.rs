//! rowser-image: PNG decoding to premultiplied BGRA pixels, from scratch.
//!
//! Implements RFC 2083 (PNG) on top of a hand-rolled RFC 1951 (DEFLATE) /
//! RFC 1950 (zlib) decoder. Supports every PNG color type (grayscale, RGB,
//! palette, grayscale+alpha, RGBA), bit depths 1/2/4/8/16, all five scanline
//! filters, Adam7 interlacing and tRNS transparency. Chunk CRCs are not
//! verified; the zlib adler32 trailer is.

mod inflate;

/// A decoded image. `pixels` is `width * height * 4` bytes of premultiplied
/// BGRA (byte order B, G, R, A), row-major from the top-left — the layout
/// GPUI's `RenderImage` expects.
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Ways PNG decoding can fail.
#[derive(Debug)]
pub enum DecodeError {
    /// not a PNG file (bad magic or far too short)
    NotPng,
    /// input ended unexpectedly
    Truncated,
    /// a required chunk is absent
    Missing(&'static str),
    /// a spec feature we deliberately reject
    Unsupported(&'static str),
    /// malformed PNG structure or deflate/zlib stream
    Invalid(&'static str),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::NotPng => write!(f, "not a PNG file"),
            DecodeError::Truncated => write!(f, "truncated PNG"),
            DecodeError::Missing(s) => write!(f, "missing {s}"),
            DecodeError::Unsupported(s) => write!(f, "unsupported: {s}"),
            DecodeError::Invalid(s) => write!(f, "invalid PNG data: {s}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Decode a PNG file into premultiplied BGRA pixels.
pub fn decode(bytes: &[u8]) -> Result<Image, DecodeError> {
    const SIG: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 8 || &bytes[..8] != SIG {
        return Err(DecodeError::NotPng);
    }
    let mut cur = Cursor { d: &bytes[8..], pos: 0 };
    let mut ihdr: Option<Ihdr> = None;
    let mut plte: Vec<[u8; 3]> = Vec::new();
    let mut trns_raw: Vec<u8> = Vec::new();
    let mut idat: Vec<u8> = Vec::new();
    let mut first = true;
    while cur.remaining() >= 8 {
        let len = cur.u32()? as usize;
        let ty: [u8; 4] = cur.take(4)?.try_into().unwrap();
        let data = cur.take(len)?;
        cur.take(4)?; // CRC, not verified
        if first && ty != *b"IHDR" {
            return Err(DecodeError::Missing("IHDR must be first chunk"));
        }
        first = false;
        match &ty {
            b"IHDR" => {
                if ihdr.is_some() {
                    return Err(DecodeError::Invalid("duplicate IHDR"));
                }
                ihdr = Some(Ihdr::parse(data)?);
            }
            b"PLTE" => {
                if data.len() % 3 != 0 || data.len() > 256 * 3 {
                    return Err(DecodeError::Invalid("bad PLTE length"));
                }
                plte = data.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect();
            }
            b"tRNS" => trns_raw = data.to_vec(),
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ => {
                // Unknown chunk: critical (uppercase first letter) → error,
                // ancillary → skip.
                if ty[0].is_ascii_uppercase() {
                    return Err(DecodeError::Unsupported("unknown critical chunk"));
                }
            }
        }
    }
    let ihdr = ihdr.ok_or(DecodeError::Missing("IHDR"))?;
    if idat.is_empty() {
        return Err(DecodeError::Missing("IDAT"));
    }
    if ihdr.color == 3 && plte.is_empty() {
        return Err(DecodeError::Missing("PLTE"));
    }

    let trns = match ihdr.color {
        0 if trns_raw.len() >= 2 => Trns::Gray(be16(&trns_raw, 0)),
        2 if trns_raw.len() >= 6 => {
            Trns::Rgb([be16(&trns_raw, 0), be16(&trns_raw, 2), be16(&trns_raw, 4)])
        }
        3 if !trns_raw.is_empty() => Trns::Palette(trns_raw.as_slice()),
        _ => Trns::None,
    };
    let fmt = Fmt {
        color: ihdr.color,
        depth: ihdr.depth,
        channels: match ihdr.color {
            0 | 3 => 1,
            2 => 3,
            4 => 2,
            _ => 4,
        },
        palette: &plte,
        trns,
    };

    let raw = inflate::zlib_decode(&idat).map_err(DecodeError::Invalid)?;
    let mut cur = Cursor { d: raw.as_slice(), pos: 0 };
    let (w, h) = (ihdr.w as usize, ihdr.h as usize);
    let mut pixels = vec![0u8; w * h * 4];
    if ihdr.interlace == 0 {
        decode_rows(&mut cur, w, h, &fmt, &mut |x, y, c| {
            put(&mut pixels, w, x, y, c);
        })?;
    } else {
        // Adam7: seven passes, each a small image of its own (RFC 2083 §4.1.8).
        for &(x0, y0, dx, dy) in &ADAM7 {
            let pw = if w > x0 { (w - x0 + dx - 1) / dx } else { 0 };
            let ph = if h > y0 { (h - y0 + dy - 1) / dy } else { 0 };
            if pw == 0 || ph == 0 {
                continue;
            }
            decode_rows(&mut cur, pw, ph, &fmt, &mut |x, y, c| {
                put(&mut pixels, w, x0 + x * dx, y0 + y * dy, c);
            })?;
        }
    }
    Ok(Image { width: ihdr.w, height: ihdr.h, pixels })
}

/// Adam7 pass origins and strides: (x0, y0, dx, dy).
const ADAM7: [(usize, usize, usize, usize); 7] = [
    (0, 0, 8, 8),
    (4, 0, 8, 8),
    (0, 4, 4, 8),
    (2, 0, 4, 4),
    (0, 2, 2, 4),
    (1, 0, 2, 2),
    (0, 1, 1, 2),
];

/// Byte-slice cursor with truncation checks.
struct Cursor<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn remaining(&self) -> usize {
        self.d.len() - self.pos
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.remaining() < n {
            return Err(DecodeError::Truncated);
        }
        let s = &self.d[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }
}

struct Ihdr {
    w: u32,
    h: u32,
    depth: u8,
    color: u8,
    interlace: u8,
}

impl Ihdr {
    fn parse(d: &[u8]) -> Result<Self, DecodeError> {
        if d.len() != 13 {
            return Err(DecodeError::Invalid("IHDR length"));
        }
        let w = u32::from_be_bytes(d[0..4].try_into().unwrap());
        let h = u32::from_be_bytes(d[4..8].try_into().unwrap());
        let depth = d[8];
        let color = d[9];
        if d[10] != 0 {
            return Err(DecodeError::Unsupported("compression method"));
        }
        if d[11] != 0 {
            return Err(DecodeError::Unsupported("filter method"));
        }
        if d[12] > 1 {
            return Err(DecodeError::Invalid("interlace method"));
        }
        if w == 0 || h == 0 {
            return Err(DecodeError::Invalid("zero dimension"));
        }
        let ok = matches!(
            (color, depth),
            (0, 1 | 2 | 4 | 8 | 16) | (2, 8 | 16) | (3, 1 | 2 | 4 | 8) | (4, 8 | 16) | (6, 8 | 16)
        );
        if !ok {
            return Err(DecodeError::Invalid("bad color type / bit depth"));
        }
        if (w as u64).checked_mul(h as u64).and_then(|n| n.checked_mul(4)).is_none()
            || w as u64 * h as u64 > (usize::MAX / 4) as u64
        {
            return Err(DecodeError::Invalid("dimensions too large"));
        }
        Ok(Self { w, h, depth, color, interlace: d[12] })
    }
}

/// tRNS transparency, resolved per color type.
#[derive(Clone, Copy)]
enum Trns<'a> {
    None,
    /// palette index → alpha (missing entries are opaque)
    Palette(&'a [u8]),
    /// raw gray sample value to make transparent
    Gray(u16),
    /// raw RGB sample values to make transparent
    Rgb([u16; 3]),
}

/// Pixel format of the PNG being decoded.
struct Fmt<'a> {
    color: u8,
    depth: u8,
    channels: usize,
    palette: &'a [[u8; 3]],
    trns: Trns<'a>,
}

fn be16(d: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([d[i], d[i + 1]])
}

/// Extract the `i`-th sample of `depth` bits from a packed scanline.
/// Samples are stored MSB-first for depths < 8, big-endian for 16.
fn sample(row: &[u8], i: usize, depth: u8) -> u16 {
    match depth {
        1 => ((row[i >> 3] >> (7 - (i & 7))) & 1) as u16,
        2 => ((row[i >> 2] >> (6 - 2 * (i & 3))) & 3) as u16,
        4 => ((row[i >> 1] >> (4 - 4 * (i & 1))) & 15) as u16,
        8 => row[i] as u16,
        _ => u16::from_be_bytes([row[2 * i], row[2 * i + 1]]),
    }
}

/// Scale a raw sample to 8 bits (16-bit: keep the most significant byte).
fn scale8(v: u16, depth: u8) -> u8 {
    match depth {
        1 => (v * 255) as u8,
        2 => (v * 85) as u8,
        4 => (v * 17) as u8,
        8 => v as u8,
        _ => (v >> 8) as u8,
    }
}

/// Read `ph` filtered scanlines of `pw` pixels, unfilter them, expand each
/// to RGBA8 and hand each pixel to `sink(pass_x, pass_y, (r,g,b,a))`.
fn decode_rows(
    cur: &mut Cursor,
    pw: usize,
    ph: usize,
    fmt: &Fmt,
    sink: &mut dyn FnMut(usize, usize, (u8, u8, u8, u8)),
) -> Result<(), DecodeError> {
    let rowbytes = (pw * fmt.channels * fmt.depth as usize + 7) / 8;
    // filter offset = bytes per complete pixel, rounded up to 1
    let bpp = ((fmt.channels * fmt.depth as usize) + 7) / 8;
    let mut prev = vec![0u8; rowbytes];
    let mut row = vec![0u8; rowbytes];
    let mut rgba = vec![(0u8, 0u8, 0u8, 0u8); pw];
    for y in 0..ph {
        let f = cur.u8()?;
        let raw = cur.take(rowbytes)?;
        unfilter(f, raw, &prev, bpp, &mut row)?;
        expand(&row, pw, fmt, &mut rgba)?;
        for x in 0..pw {
            sink(x, y, rgba[x]);
        }
        prev.copy_from_slice(&row);
    }
    Ok(())
}

/// Reverse one scanline's filter into `out` (RFC 2083 §6).
/// `raw` = filtered bytes, `prev` = previous reconstructed row (zeros at top).
fn unfilter(
    f: u8,
    raw: &[u8],
    prev: &[u8],
    bpp: usize,
    out: &mut [u8],
) -> Result<(), DecodeError> {
    match f {
        0 => out.copy_from_slice(raw),
        1 => {
            for i in 0..raw.len() {
                let a = if i >= bpp { out[i - bpp] } else { 0 };
                out[i] = raw[i].wrapping_add(a);
            }
        }
        2 => {
            for i in 0..raw.len() {
                out[i] = raw[i].wrapping_add(prev[i]);
            }
        }
        3 => {
            for i in 0..raw.len() {
                let a = if i >= bpp { out[i - bpp] as u16 } else { 0 };
                let b = prev[i] as u16;
                out[i] = raw[i].wrapping_add(((a + b) / 2) as u8);
            }
        }
        4 => {
            for i in 0..raw.len() {
                let a = if i >= bpp { out[i - bpp] } else { 0 };
                let c = if i >= bpp { prev[i - bpp] } else { 0 };
                out[i] = raw[i].wrapping_add(paeth(a, prev[i], c));
            }
        }
        _ => return Err(DecodeError::Invalid("bad filter type")),
    }
    Ok(())
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let (a, b, c) = (a as i32, b as i32, c as i32);
    let p = a + b - c;
    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
    if pa <= pb && pa <= pc {
        a as u8
    } else if pb <= pc {
        b as u8
    } else {
        c as u8
    }
}

/// Expand one reconstructed scanline into `w` RGBA8 pixels.
fn expand(
    row: &[u8],
    w: usize,
    fmt: &Fmt,
    out: &mut [(u8, u8, u8, u8)],
) -> Result<(), DecodeError> {
    for i in 0..w {
        out[i] = match fmt.color {
            0 => {
                let v = sample(row, i, fmt.depth);
                let a = match fmt.trns {
                    Trns::Gray(t) if v == t => 0,
                    _ => 255,
                };
                let g = scale8(v, fmt.depth);
                (g, g, g, a)
            }
            2 => {
                let (r, g, b) = (
                    sample(row, 3 * i, fmt.depth),
                    sample(row, 3 * i + 1, fmt.depth),
                    sample(row, 3 * i + 2, fmt.depth),
                );
                let a = match fmt.trns {
                    Trns::Rgb(t) if [r, g, b] == t => 0,
                    _ => 255,
                };
                (
                    scale8(r, fmt.depth),
                    scale8(g, fmt.depth),
                    scale8(b, fmt.depth),
                    a,
                )
            }
            3 => {
                let idx = sample(row, i, fmt.depth) as usize;
                let Some(&[r, g, b]) = fmt.palette.get(idx) else {
                    return Err(DecodeError::Invalid("palette index out of range"));
                };
                let a = match fmt.trns {
                    Trns::Palette(t) => t.get(idx).copied().unwrap_or(255),
                    _ => 255,
                };
                (r, g, b, a)
            }
            4 => {
                let v = sample(row, 2 * i, fmt.depth);
                let a = sample(row, 2 * i + 1, fmt.depth);
                let g = scale8(v, fmt.depth);
                (g, g, g, scale8(a, fmt.depth))
            }
            _ => {
                // color type 6
                (
                    scale8(sample(row, 4 * i, fmt.depth), fmt.depth),
                    scale8(sample(row, 4 * i + 1, fmt.depth), fmt.depth),
                    scale8(sample(row, 4 * i + 2, fmt.depth), fmt.depth),
                    scale8(sample(row, 4 * i + 3, fmt.depth), fmt.depth),
                )
            }
        };
    }
    Ok(())
}

/// Write one pixel as premultiplied BGRA at (x, y) in a `w`-wide buffer.
fn put(px: &mut [u8], w: usize, x: usize, y: usize, (r, g, b, a): (u8, u8, u8, u8)) {
    let i = (y * w + x) * 4;
    let a = a as u32;
    px[i] = ((b as u32 * a + 127) / 255) as u8;
    px[i + 1] = ((g as u32 * a + 127) / 255) as u8;
    px[i + 2] = ((r as u32 * a + 127) / 255) as u8;
    px[i + 3] = a as u8;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inflate::{zlib_decode, CL_ORDER, DIST_BASE, DIST_EXTRA, LEN_BASE, LEN_EXTRA};

    // ---------- fixture encoder: minimal PNG/DEFLATE writers ----------

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }

    fn adler32(data: &[u8]) -> u32 {
        let (mut a, mut s) = (1u32, 0u32);
        for &x in data {
            a = (a + x as u32) % 65521;
            s = (s + a) % 65521;
        }
        (s << 16) | a
    }

    fn chunk(ty: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend((data.len() as u32).to_be_bytes());
        let mut c = Vec::with_capacity(4 + data.len());
        c.extend_from_slice(ty);
        c.extend_from_slice(data);
        v.extend_from_slice(&c);
        v.extend(crc32(&c).to_be_bytes());
        v
    }

    fn ihdr(w: u32, h: u32, depth: u8, color: u8, interlace: u8) -> Vec<u8> {
        let mut d = w.to_be_bytes().to_vec();
        d.extend(h.to_be_bytes());
        d.extend([depth, color, 0, 0, interlace]);
        d
    }

    /// Assemble a PNG file from (type, data) chunks; IEND appended.
    fn make_png(chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        for (ty, d) in chunks {
            v.extend(chunk(ty, d));
        }
        v.extend(chunk(b"IEND", &[]));
        v
    }

    /// LSB-first bit writer for DEFLATE streams.
    struct BitW {
        out: Vec<u8>,
        bit: u8,
    }
    impl BitW {
        fn new() -> Self {
            Self { out: Vec::new(), bit: 0 }
        }
        /// write `n` bits of `v`, LSB first
        fn bits(&mut self, v: u32, n: u8) {
            for i in 0..n {
                if self.bit == 0 {
                    self.out.push(0);
                }
                if (v >> i) & 1 != 0 {
                    *self.out.last_mut().unwrap() |= 1 << self.bit;
                }
                self.bit = (self.bit + 1) & 7;
            }
        }
        /// write a Huffman code: `len` bits of `code`, MSB first
        fn code(&mut self, code: u16, len: u8) {
            for i in (0..len).rev() {
                self.bits(((code >> i) & 1) as u32, 1);
            }
        }
    }

    /// Canonical Huffman codes for a length table (RFC 1951 §3.2.2).
    fn canonical(lengths: &[u8]) -> Vec<(u16, u8)> {
        let mut bl = [0u16; 16];
        for &l in lengths {
            bl[l as usize] += 1;
        }
        bl[0] = 0;
        let mut next = [0u16; 16];
        let mut code = 0u16;
        for b in 1..16 {
            code = (code + bl[b - 1]) << 1;
            next[b] = code;
        }
        lengths
            .iter()
            .map(|&l| {
                if l == 0 {
                    (0, 0)
                } else {
                    let c = next[l as usize];
                    next[l as usize] += 1;
                    (c, l)
                }
            })
            .collect()
    }

    fn fixed_lit_lengths() -> Vec<u8> {
        let mut l = vec![8u8; 288];
        for x in &mut l[144..256] {
            *x = 9;
        }
        for x in &mut l[256..280] {
            *x = 7;
        }
        l
    }

    fn emit(w: &mut BitW, codes: &[(u16, u8)], sym: usize) {
        let (c, l) = codes[sym];
        debug_assert!(l > 0, "symbol {sym} has no code");
        w.code(c, l);
    }

    fn find_sym(base: &[u16], v: usize) -> usize {
        let mut i = 0;
        while i + 1 < base.len() && base[i + 1] as usize <= v {
            i += 1;
        }
        i
    }

    fn emit_match(w: &mut BitW, lc: &[(u16, u8)], dc: &[(u16, u8)], len: usize, dist: usize) {
        let ls = find_sym(&LEN_BASE, len);
        emit(w, lc, 257 + ls);
        w.bits((len - LEN_BASE[ls] as usize) as u32, LEN_EXTRA[ls]);
        let ds = find_sym(&DIST_BASE, dist);
        emit(w, dc, ds);
        w.bits((dist - DIST_BASE[ds] as usize) as u32, DIST_EXTRA[ds]);
    }

    /// Greedy longest-match search in the last `window` bytes.
    fn find_match(data: &[u8], i: usize, window: usize) -> (usize, usize) {
        let mut best = (0usize, 0usize);
        for j in i.saturating_sub(window)..i {
            let mut l = 0;
            while i + l < data.len() && l < 258 && data[j + l] == data[i + l] {
                l += 1;
            }
            if l > best.0 {
                best = (l, i - j);
            }
        }
        if best.0 >= 3 {
            best
        } else {
            (0, 0)
        }
    }

    /// Emit literals + LZ77 matches + EOB using the given code tables.
    fn emit_payload(w: &mut BitW, data: &[u8], lc: &[(u16, u8)], dc: &[(u16, u8)]) {
        let mut i = 0;
        while i < data.len() {
            let (len, dist) = find_match(data, i, 1024);
            if len >= 3 {
                emit_match(w, lc, dc, len, dist);
                i += len;
            } else {
                emit(w, lc, data[i] as usize);
                i += 1;
            }
        }
        emit(w, lc, 256);
    }

    /// BTYPE=01 fixed-Huffman DEFLATE stream with real LZ77 matches.
    fn deflate_fixed(data: &[u8]) -> Vec<u8> {
        let lc = canonical(&fixed_lit_lengths());
        let dc = canonical(&[5u8; 32]);
        let mut w = BitW::new();
        w.bits(1, 1);
        w.bits(1, 2); // BTYPE=01
        emit_payload(&mut w, data, &lc, &dc);
        w.out
    }

    /// Emit a code-length sequence, using 16/17/18 run codes when profitable.
    fn emit_cl_seq(w: &mut BitW, clc: &[(u16, u8)], lens: &[u8]) {
        let mut i = 0;
        while i < lens.len() {
            let v = lens[i];
            let mut run = 1;
            while i + run < lens.len() && lens[i + run] == v {
                run += 1;
            }
            if v == 0 {
                let mut n = run;
                while n >= 11 {
                    let r = n.min(138);
                    emit(w, clc, 18);
                    w.bits((r - 11) as u32, 7);
                    n -= r;
                }
                if n >= 3 {
                    emit(w, clc, 17);
                    w.bits((n - 3) as u32, 3);
                    n = 0;
                }
                while n > 0 {
                    emit(w, clc, 0);
                    n -= 1;
                }
            } else {
                emit(w, clc, v as usize);
                let mut n = run - 1;
                while n >= 3 {
                    let r = n.min(6);
                    emit(w, clc, 16);
                    w.bits((r - 3) as u32, 2);
                    n -= r;
                }
                while n > 0 {
                    emit(w, clc, v as usize);
                    n -= 1;
                }
            }
            i += run;
        }
    }

    /// BTYPE=10 dynamic block carrying the fixed tables verbatim (no repeats).
    /// Works for arbitrary data.
    fn deflate_dynamic(data: &[u8]) -> Vec<u8> {
        let lit = fixed_lit_lengths();
        let dist = [5u8; 32];
        // code-length code: symbols {5,7,8,9} at length 2 (complete tree)
        let mut cl = [0u8; 19];
        for &s in &[5usize, 7, 8, 9] {
            cl[s] = 2;
        }
        let hclen = 10; // covers CL_ORDER positions of 8,7,9,...,5
        let clc = canonical(&cl);
        let mut w = BitW::new();
        w.bits(1, 1);
        w.bits(2, 2); // BTYPE=10
        w.bits((288 - 257) as u32, 5);
        w.bits((32 - 1) as u32, 5);
        w.bits((hclen - 4) as u32, 4);
        for &i in CL_ORDER.iter().take(hclen) {
            w.bits(cl[i] as u32, 3);
        }
        for l in lit.iter().chain(dist.iter()) {
            emit(&mut w, &clc, *l as usize);
        }
        emit_payload(&mut w, data, &canonical(&lit), &canonical(&dist));
        w.out
    }

    /// BTYPE=10 dynamic block with 16/17/18 code-length repeat codes.
    /// Only valid for data using byte values 0..=3 (the test alphabet below).
    fn deflate_dynamic_rle(data: &[u8]) -> Vec<u8> {
        debug_assert!(data.iter().all(|&b| b <= 3));
        // lit: syms 0-3 len 3, syms 4..=255 unused (252 zeros → 18s),
        //      256..=285 len 6 (EOB + all length codes), 286-287 unused.
        let mut lit = vec![0u8; 288];
        for x in &mut lit[0..4] {
            *x = 3;
        }
        for x in &mut lit[256..286] {
            *x = 6;
        }
        // dist: syms 0-25 len 5, then 6 zeros → one 17
        let mut dist = vec![0u8; 32];
        for x in &mut dist[0..26] {
            *x = 5;
        }
        // cl code for {16,18,17,6,3,5,0} — lengths 1,2,3,4,5,6,6 (complete)
        let mut cl = [0u8; 19];
        cl[16] = 1;
        cl[18] = 2;
        cl[17] = 3;
        cl[6] = 4;
        cl[3] = 5;
        cl[5] = 6;
        cl[0] = 6;
        let hclen = 14; // last needed CL_ORDER entry is symbol 3 (position 13)
        let clc = canonical(&cl);
        let mut w = BitW::new();
        w.bits(1, 1);
        w.bits(2, 2);
        w.bits((288 - 257) as u32, 5);
        w.bits((32 - 1) as u32, 5);
        w.bits((hclen - 4) as u32, 4);
        for &i in CL_ORDER.iter().take(hclen) {
            w.bits(cl[i] as u32, 3);
        }
        let all: Vec<u8> = lit.iter().chain(dist.iter()).copied().collect();
        emit_cl_seq(&mut w, &clc, &all);
        emit_payload(&mut w, data, &canonical(&lit), &canonical(&dist));
        w.out
    }

    /// BTYPE=00 stored DEFLATE stream.
    fn deflate_stored(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut chunks: Vec<&[u8]> = data.chunks(65535).collect();
        if chunks.is_empty() {
            chunks.push(&[]);
        }
        for (i, c) in chunks.iter().enumerate() {
            out.push(u8::from(i + 1 == chunks.len())); // BFINAL, BTYPE=00
            let len = c.len() as u16;
            out.extend(len.to_le_bytes());
            out.extend((!len).to_le_bytes());
            out.extend_from_slice(c);
        }
        out
    }

    /// Wrap a DEFLATE stream in a zlib envelope.
    fn zlib(deflate: &[u8], data: &[u8]) -> Vec<u8> {
        let mut v = vec![0x78, 0x01];
        v.extend_from_slice(deflate);
        v.extend(adler32(data).to_be_bytes());
        v
    }

    // ---------- scanline packing / forward filtering ----------

    /// Pack one row of `depth`-bit samples, MSB first for sub-byte depths.
    fn pack_samples(samples: &[u16], depth: u8) -> Vec<u8> {
        match depth {
            8 => samples.iter().map(|&s| s as u8).collect(),
            16 => samples.iter().flat_map(|&s| s.to_be_bytes()).collect(),
            d => {
                let mut v = Vec::new();
                let (mut acc, mut n) = (0u8, 0u8);
                for &s in samples {
                    acc = (acc << d) | s as u8;
                    n += d;
                    if n == 8 {
                        v.push(acc);
                        acc = 0;
                        n = 0;
                    }
                }
                if n > 0 {
                    v.push(acc << (8 - n));
                }
                v
            }
        }
    }

    /// Forward-filter a row (test-side mirror of the decoder's unfilter).
    fn filter_row(f: u8, row: &[u8], prev: &[u8], bpp: usize) -> Vec<u8> {
        let a = |i: usize| if i >= bpp { row[i - bpp] } else { 0 };
        let c = |i: usize| if i >= bpp { prev[i - bpp] } else { 0 };
        (0..row.len())
            .map(|i| {
                let p = match f {
                    0 => 0,
                    1 => a(i),
                    2 => prev[i],
                    3 => ((a(i) as u16 + prev[i] as u16) / 2) as u8,
                    _ => paeth(a(i), prev[i], c(i)),
                };
                row[i].wrapping_sub(p)
            })
            .collect()
    }

    /// (filter byte + filtered data) per row, concatenated.
    fn scanlines(rows: &[Vec<u8>], filters: &[u8], bpp: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut prev = vec![0u8; rows.first().map_or(0, |r| r.len())];
        for (row, &f) in rows.iter().zip(filters) {
            out.push(f);
            out.extend(filter_row(f, row, &prev, bpp));
            prev = row.clone();
        }
        out
    }

    /// IDAT payload for a non-interlaced image: sample rows + filters.
    fn stream(rows: &[Vec<u16>], depth: u8, filters: &[u8], bpp: usize) -> Vec<u8> {
        let raw: Vec<Vec<u8>> = rows.iter().map(|r| pack_samples(r, depth)).collect();
        scanlines(&raw, filters, bpp)
    }

    /// IDAT payload for an Adam7 image. `pix(x,y)` → channel samples.
    fn stream_adam7(
        w: usize,
        h: usize,
        depth: u8,
        pix: &dyn Fn(usize, usize) -> Vec<u16>,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        for &(x0, y0, dx, dy) in &ADAM7 {
            let pw = if w > x0 { (w - x0 + dx - 1) / dx } else { 0 };
            let ph = if h > y0 { (h - y0 + dy - 1) / dy } else { 0 };
            if pw == 0 || ph == 0 {
                continue;
            }
            for j in 0..ph {
                out.push(0); // filter: none
                let mut samples = Vec::new();
                for i in 0..pw {
                    samples.extend(pix(x0 + i * dx, y0 + j * dy));
                }
                out.extend(pack_samples(&samples, depth));
            }
        }
        out
    }

    // ---------- expected output helpers ----------

    fn pm(c: u8, a: u8) -> u8 {
        ((c as u32 * a as u32 + 127) / 255) as u8
    }

    /// RGBA8 tuples → premultiplied BGRA bytes.
    fn bgra(px: &[(u8, u8, u8, u8)]) -> Vec<u8> {
        px.iter().flat_map(|&(r, g, b, a)| [pm(b, a), pm(g, a), pm(r, a), a]).collect()
    }

    /// deterministic colorful pixel
    fn pix(i: usize) -> (u8, u8, u8, u8) {
        let v = (i as u32).wrapping_mul(0x9E37_79B1) >> 8;
        (
            (v & 0xFF) as u8,
            ((v >> 4) & 0xFF) as u8,
            (v >> 12) as u8,
            [255, 128, 64, 0, 200][i % 5],
        )
    }

    fn png_rgba8(w: usize, h: usize, filters: &[u8], def: fn(&[u8]) -> Vec<u8>) -> Vec<u8> {
        let rows: Vec<Vec<u16>> = (0..h)
            .map(|y| {
                (0..w)
                    .flat_map(|x| {
                        let (r, g, b, a) = pix(y * w + x);
                        [r as u16, g as u16, b as u16, a as u16]
                    })
                    .collect()
            })
            .collect();
        let data = stream(&rows, 8, filters, 4);
        make_png(&[
            (b"IHDR", ihdr(w as u32, h as u32, 8, 6, 0)),
            (b"IDAT", zlib(&def(&data), &data)),
        ])
    }

    // ---------- tests ----------

    #[test]
    fn rgba8_all_filters() {
        let (w, h) = (9, 6);
        let filters = [0, 1, 2, 3, 4, 2]; // every filter type exercised
        let png = png_rgba8(w, h, &filters, deflate_fixed);
        let img = decode(&png).unwrap();
        assert_eq!(img.width, 9);
        assert_eq!(img.height, 6);
        let want: Vec<(u8, u8, u8, u8)> = (0..w * h).map(pix).collect();
        assert_eq!(img.pixels, bgra(&want));
        // spot check: pixel 0 has alpha 255 → unpremultiplied values
        let (r, g, b, _) = pix(0);
        assert_eq!(&img.pixels[0..4], &[b, g, r, 255]);
    }

    #[test]
    fn rgba8_dynamic_huffman() {
        let png = png_rgba8(11, 5, &[4; 5], deflate_dynamic);
        let img = decode(&png).unwrap();
        let want: Vec<(u8, u8, u8, u8)> = (0..55).map(pix).collect();
        assert_eq!(img.pixels, bgra(&want));
    }

    #[test]
    fn rgb8_stored() {
        let (w, h) = (4, 2);
        let rows: Vec<Vec<u16>> = (0..h)
            .map(|y| {
                (0..w)
                    .flat_map(|x| {
                        let (r, g, b, _) = pix(y * w + x);
                        [r as u16, g as u16, b as u16]
                    })
                    .collect()
            })
            .collect();
        let data = stream(&rows, 8, &[1, 3], 3);
        let png = make_png(&[
            (b"IHDR", ihdr(w as u32, h as u32, 8, 2, 0)),
            (b"IDAT", zlib(&deflate_stored(&data), &data)),
        ]);
        let img = decode(&png).unwrap();
        let want: Vec<(u8, u8, u8, u8)> =
            (0..w * h).map(|i| { let (r, g, b, _) = pix(i); (r, g, b, 255) }).collect();
        assert_eq!(img.pixels, bgra(&want));
    }

    #[test]
    fn rgb8_trns() {
        // 2x1, second pixel is the transparent color
        let (r0, g0, b0) = (10u8, 20, 30);
        let (r1, g1, b1) = (200u8, 100, 50);
        let row = vec![r0, g0, b0, r1, g1, b1];
        let data = scanlines(&[row], &[0], 3);
        let png = make_png(&[
            (b"IHDR", ihdr(2, 1, 8, 2, 0)),
            (b"tRNS", vec![0, r1, 0, g1, 0, b1]),
            (b"IDAT", zlib(&deflate_fixed(&data), &data)),
        ]);
        let img = decode(&png).unwrap();
        assert_eq!(&img.pixels[0..4], &[b0, g0, r0, 255]);
        assert_eq!(&img.pixels[4..8], &[0, 0, 0, 0]); // transparent → premult 0
    }

    #[test]
    fn palette8_trns() {
        let indices: Vec<u16> = vec![0, 1, 2, 3, 2, 1, 0, 3];
        let data = stream(&[indices.clone()], 8, &[0], 1);
        let plte = vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 10, 20, 30];
        let trns = vec![255u8, 128, 0]; // index 3 defaults to opaque
        let png = make_png(&[
            (b"IHDR", ihdr(8, 1, 8, 3, 0)),
            (b"PLTE", plte),
            (b"tRNS", trns),
            (b"IDAT", zlib(&deflate_fixed(&data), &data)),
        ]);
        let img = decode(&png).unwrap();
        assert_eq!(&img.pixels[0..4], &[0, 0, 255, 255]); // red, opaque
        assert_eq!(&img.pixels[4..8], &[0, pm(255, 128), 0, 128]); // green, half
        assert_eq!(&img.pixels[8..12], &[0, 0, 0, 0]); // blue, transparent
        assert_eq!(&img.pixels[12..16], &[30, 20, 10, 255]); // (10,20,30) opaque
    }

    #[test]
    fn palette4_packed() {
        // 5 pixels of 4-bit indices → 3 bytes/row, last nibble padding
        let indices: Vec<u16> = vec![0, 7, 15, 1, 9];
        let data = stream(&[indices], 4, &[0], 1);
        let mut plte = Vec::new();
        for i in 0..16u8 {
            plte.extend([i * 16, 255 - i * 16, i * 8]);
        }
        let png = make_png(&[
            (b"IHDR", ihdr(5, 1, 4, 3, 0)),
            (b"PLTE", plte),
            (b"IDAT", zlib(&deflate_fixed(&data), &data)),
        ]);
        let img = decode(&png).unwrap();
        let check = |px_i: usize, idx: u8| {
            let (r, g, b) = (idx * 16, 255 - idx * 16, idx * 8);
            assert_eq!(&img.pixels[px_i * 4..px_i * 4 + 4], &[b, g, r, 255]);
        };
        check(0, 0);
        check(1, 7);
        check(2, 15);
        check(3, 1);
        check(4, 9);
    }

    #[test]
    fn gray_depths() {
        // 8-bit gray, two rows (Up + Sub filters)
        let g: Vec<u16> = (0..10).map(|i| (i * 25) as u16).collect();
        let g2: Vec<u16> = (0..10).map(|i| (i * 25 + 7) as u16).collect();
        let data = stream(&[g.clone(), g2.clone()], 8, &[2, 1], 1);
        let png = make_png(&[
            (b"IHDR", ihdr(10, 2, 8, 0, 0)),
            (b"IDAT", zlib(&deflate_fixed(&data), &data)),
        ]);
        let img = decode(&png).unwrap();
        for (i, &v) in g.iter().chain(&g2).enumerate() {
            let v = v as u8;
            assert_eq!(&img.pixels[i * 4..i * 4 + 4], &[v, v, v, 255], "pixel {i}");
        }
    }

    #[test]
    fn gray_subbyte_depths() {
        for (depth, vals) in [
            (1u8, vec![0u16, 1, 1, 0, 1, 0, 0, 1, 1]),
            (2, vec![0u16, 1, 2, 3, 3, 2, 1, 0, 2]),
            (4, vec![0u16, 5, 10, 15, 3, 9, 12, 1, 7]),
        ] {
            let data = stream(&[vals.clone()], depth, &[0], 1);
            let png = make_png(&[
                (b"IHDR", ihdr(vals.len() as u32, 1, depth, 0, 0)),
                (b"IDAT", zlib(&deflate_fixed(&data), &data)),
            ]);
            let img = decode(&png).unwrap();
            let max = (1u16 << depth) - 1;
            for (i, &v) in vals.iter().enumerate() {
                let g = ((v * 255) / max) as u8;
                assert_eq!(&img.pixels[i * 4..i * 4 + 4], &[g, g, g, 255], "d{depth} px{i}");
            }
        }
    }

    #[test]
    fn gray16() {
        let vals: Vec<u16> = vec![0x0000, 0x1234, 0x8000, 0xFFFF, 0x00FF];
        let data = stream(&[vals.clone()], 16, &[0], 2);
        let png = make_png(&[
            (b"IHDR", ihdr(5, 1, 16, 0, 0)),
            (b"IDAT", zlib(&deflate_fixed(&data), &data)),
        ]);
        let img = decode(&png).unwrap();
        for (i, &v) in vals.iter().enumerate() {
            let g = (v >> 8) as u8;
            assert_eq!(&img.pixels[i * 4..i * 4 + 4], &[g, g, g, 255], "px{i}");
        }
    }

    #[test]
    fn gray_alpha8() {
        let samples: Vec<u16> = vec![200, 255, 100, 128, 50, 0]; // (g,a) × 3
        let data = stream(&[samples], 8, &[0], 2);
        let png = make_png(&[
            (b"IHDR", ihdr(3, 1, 8, 4, 0)),
            (b"IDAT", zlib(&deflate_fixed(&data), &data)),
        ]);
        let img = decode(&png).unwrap();
        assert_eq!(&img.pixels[0..4], &[200, 200, 200, 255]);
        let g = pm(100, 128);
        assert_eq!(&img.pixels[4..8], &[g, g, g, 128]);
        assert_eq!(&img.pixels[8..12], &[0, 0, 0, 0]);
    }

    #[test]
    fn rgb16() {
        let samples: Vec<u16> = vec![0xFF00, 0x1234, 0x00FF, 0x0000, 0x8000, 0xFFFF];
        let data = stream(&[samples.clone()], 16, &[0], 6);
        let png = make_png(&[
            (b"IHDR", ihdr(2, 1, 16, 2, 0)),
            (b"IDAT", zlib(&deflate_fixed(&data), &data)),
        ]);
        let img = decode(&png).unwrap();
        assert_eq!(&img.pixels[0..4], &[0x00, 0x12, 0xFF, 255]); // r ff00→ff, g 1234→12, b 00ff→00
        assert_eq!(&img.pixels[4..8], &[0xFF, 0x80, 0x00, 255]);
    }

    #[test]
    fn rgba16() {
        let samples: Vec<u16> = vec![0xFF00, 0x8000, 0x0100, 0x8080];
        let data = stream(&[samples], 16, &[0], 8);
        let png = make_png(&[
            (b"IHDR", ihdr(1, 1, 16, 6, 0)),
            (b"IDAT", zlib(&deflate_fixed(&data), &data)),
        ]);
        let img = decode(&png).unwrap();
        // r=0xFF g=0x80 b=0x01 a=0x80 → premult: c*128/255 ≈ (c+1)/2 for odd c? exact via pm
        let a = 0x80u8;
        assert_eq!(
            img.pixels,
            vec![pm(0x01, a), pm(0x80, a), pm(0xFF, a), a]
        );
    }

    #[test]
    fn interlaced_rgba8() {
        let (w, h) = (13usize, 11usize); // deliberately not multiples of 8
        let pixels: Vec<(u8, u8, u8, u8)> = (0..w * h).map(pix).collect();
        let get = |x: usize, y: usize| {
            let (r, g, b, a) = pixels[y * w + x];
            vec![r as u16, g as u16, b as u16, a as u16]
        };
        let data = stream_adam7(w, h, 8, &get);
        let png = make_png(&[
            (b"IHDR", ihdr(w as u32, h as u32, 8, 6, 1)),
            (b"IDAT", zlib(&deflate_fixed(&data), &data)),
        ]);
        let img = decode(&png).unwrap();
        assert_eq!(img.pixels, bgra(&pixels));
    }

    #[test]
    fn interlaced_gray4() {
        let (w, h) = (9usize, 7usize);
        let vals: Vec<u16> = (0..w * h).map(|i| ((i * 5 + 3) % 16) as u16).collect();
        let get = |x: usize, y: usize| vec![vals[y * w + x]];
        let data = stream_adam7(w, h, 4, &get);
        let png = make_png(&[
            (b"IHDR", ihdr(w as u32, h as u32, 4, 0, 1)),
            (b"IDAT", zlib(&deflate_fixed(&data), &data)),
        ]);
        let img = decode(&png).unwrap();
        for (i, &v) in vals.iter().enumerate() {
            let g = (v * 17) as u8;
            assert_eq!(&img.pixels[i * 4..i * 4 + 4], &[g, g, g, 255], "px{i}");
        }
    }

    // ---------- deflate-level tests ----------

    #[test]
    fn deflate_roundtrips() {
        // data with repeats (matches) and full byte range (literals)
        let mut data = Vec::new();
        for i in 0..600usize {
            data.push((i % 256) as u8);
            if i % 7 == 0 {
                data.extend_from_slice(&[7, 7, 7, 7, 7, 7, 7]);
            }
            if i % 11 == 0 {
                let prefix: Vec<u8> = data[..i.min(40)].to_vec(); // long-range match
                data.extend_from_slice(&prefix);
            }
        }
        let defs: [fn(&[u8]) -> Vec<u8>; 3] = [deflate_stored, deflate_fixed, deflate_dynamic];
        for def in defs {
            assert_eq!(zlib_decode(&zlib(&def(&data), &data)).unwrap(), data);
        }
    }

    #[test]
    fn deflate_dynamic_rle_decode() {
        // small alphabet → exercises code-length repeat codes 16/17/18
        let mut data = Vec::new();
        for i in 0..400usize {
            data.push((i % 4) as u8);
            if i % 5 == 0 {
                data.extend_from_slice(&[2, 2, 2, 2, 2, 2]);
            }
        }
        assert_eq!(
            zlib_decode(&zlib(&deflate_dynamic_rle(&data), &data)).unwrap(),
            data
        );
    }

    // ---------- error paths ----------

    #[test]
    fn rejects_garbage() {
        assert!(matches!(decode(b""), Err(DecodeError::NotPng)));
        assert!(matches!(decode(b"\x89PNG\r\n\x1a"), Err(DecodeError::NotPng)));
        assert!(matches!(decode(b"GIF89a......"), Err(DecodeError::NotPng)));
    }

    #[test]
    fn rejects_truncated() {
        let png = png_rgba8(4, 4, &[0; 4], deflate_fixed);
        assert!(decode(&png[..png.len() - 3]).is_err()); // inside IEND/adler
        assert!(decode(&png[..20]).is_err()); // inside IHDR
    }

    #[test]
    fn rejects_bad_filter() {
        let data = vec![9u8, 0, 0, 0, 0]; // filter 9 + one RGBA pixel
        let png = make_png(&[
            (b"IHDR", ihdr(1, 1, 8, 6, 0)),
            (b"IDAT", zlib(&deflate_fixed(&data), &data)),
        ]);
        assert!(matches!(decode(&png), Err(DecodeError::Invalid(_))));
    }

    #[test]
    fn rejects_missing_plte() {
        let data = stream(&[vec![0u16, 1, 1, 0]], 2, &[0], 1);
        let png = make_png(&[
            (b"IHDR", ihdr(4, 1, 2, 3, 0)),
            (b"IDAT", zlib(&deflate_fixed(&data), &data)),
        ]);
        assert!(matches!(decode(&png), Err(DecodeError::Missing(_))));
    }

    #[test]
    fn rejects_bad_adler() {
        let data = stream(&[vec![1u16, 2, 3, 4]], 8, &[0], 4);
        let mut z = zlib(&deflate_fixed(&data), &data);
        *z.last_mut().unwrap() ^= 0xFF; // corrupt adler32
        let png = make_png(&[(b"IHDR", ihdr(1, 1, 8, 6, 0)), (b"IDAT", z)]);
        assert!(matches!(decode(&png), Err(DecodeError::Invalid(_))));
    }

    #[test]
    fn rejects_bad_combo() {
        // bit depth 16 is not allowed for palette images
        let png = make_png(&[(b"IHDR", ihdr(1, 1, 16, 3, 0)), (b"IDAT", vec![0])]);
        assert!(matches!(decode(&png), Err(DecodeError::Invalid(_))));
    }
}
