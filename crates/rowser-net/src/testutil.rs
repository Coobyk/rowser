//! Test-only DEFLATE *encoder*, used to build compressed fixtures without
//! shelling out to gzip/python. It mirrors the decoder's canonical-Huffman
//! construction, so a green round-trip cross-checks both directions.

use crate::inflate::{DIST_BASE, DIST_EXTRA, LENGTH_BASE, LENGTH_EXTRA, adler32, crc32};

/// RFC 1951 §3.2.7 order for code-length code lengths (dup of the private
/// constant in `inflate`).
const CL_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// A compressed-block instruction for [`deflate_dynamic`].
pub(crate) enum Op {
    /// Emit one literal byte.
    Lit(u8),
    /// Emit a length/distance back-reference.
    Match { len: usize, dist: usize },
}

/// LSB-first bit sink; Huffman codes go in most-significant-bit first.
pub(crate) struct BitWriter {
    bytes: Vec<u8>,
    /// Bits used in the current (last) byte, 0..8.
    nbits: u8,
}

impl BitWriter {
    pub(crate) fn new() -> Self {
        Self { bytes: Vec::new(), nbits: 0 }
    }

    /// Append `n` bits, least-significant first (header fields, extra bits).
    pub(crate) fn push_bits(&mut self, val: u32, n: u8) {
        for i in 0..n {
            self.push_bit((val >> i) & 1);
        }
    }

    /// Append a Huffman code most-significant-bit first (RFC 1951 §3.1.1).
    pub(crate) fn push_code(&mut self, code: u16, len: u8) {
        for i in (0..len).rev() {
            self.push_bit(u32::from(code >> i) & 1);
        }
    }

    fn push_bit(&mut self, b: u32) {
        if self.nbits == 0 {
            self.bytes.push(0);
        }
        if b == 1 {
            *self.bytes.last_mut().unwrap() |= 1 << self.nbits;
        }
        self.nbits = (self.nbits + 1) % 8;
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

/// Assign canonical codes for the given code lengths (RFC 1951 §3.2.2):
/// `(code, len)` per symbol, `(0, 0)` where the symbol has no code.
fn canonical_codes(lengths: &[u8]) -> Vec<(u16, u8)> {
    let mut bl_count = [0u16; 16];
    for &l in lengths {
        bl_count[l as usize] += 1;
    }
    bl_count[0] = 0;
    let mut next_code = [0u16; 16];
    let mut code = 0u16;
    for bits in 1..16 {
        code = (code + bl_count[bits - 1]) << 1;
        next_code[bits] = code;
    }
    let mut out = vec![(0u16, 0u8); lengths.len()];
    for (sym, &l) in lengths.iter().enumerate() {
        if l != 0 {
            out[sym] = (next_code[l as usize], l);
            next_code[l as usize] += 1;
        }
    }
    out
}

/// Fixed-table literal/length code (RFC 1951 §3.2.6).
pub(crate) fn fixed_lit(sym: usize) -> (u16, u8) {
    match sym {
        0..=143 => (0x30 + sym as u16, 8),
        144..=255 => (0x190 + (sym - 144) as u16, 9),
        256..=279 => ((sym - 256) as u16, 7),
        _ => (0xc0 + (sym - 280) as u16, 8),
    }
}

/// A single fixed-Huffman block of literals only.
pub(crate) fn deflate_fixed(data: &[u8]) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.push_bits(1, 1); // BFINAL
    w.push_bits(1, 2); // BTYPE=01
    for &b in data {
        let (c, n) = fixed_lit(usize::from(b));
        w.push_code(c, n);
    }
    let (c, n) = fixed_lit(256);
    w.push_code(c, n);
    w.finish()
}

/// A single dynamic-Huffman block. Lit/len code lengths: 8 for symbols
/// 0..226, 9 for 226..286 (Kraft-complete); two 1-bit distance codes.
/// Code-length code: lengths {1: 1, 8: 2, 9: 2} — also complete.
pub(crate) fn deflate_dynamic(ops: &[Op]) -> Vec<u8> {
    let mut lit_lengths = [0u8; 286];
    for l in &mut lit_lengths[..226] {
        *l = 8;
    }
    for l in &mut lit_lengths[226..] {
        *l = 9;
    }
    let dist_lengths = [1u8; 2];
    let lit_codes = canonical_codes(&lit_lengths);
    let dist_codes = canonical_codes(&dist_lengths);

    let mut cl_lengths = [0u8; 19];
    cl_lengths[1] = 1;
    cl_lengths[8] = 2;
    cl_lengths[9] = 2;
    let cl_codes = canonical_codes(&cl_lengths);

    let mut w = BitWriter::new();
    w.push_bits(1, 1); // BFINAL
    w.push_bits(2, 2); // BTYPE=10
    w.push_bits((286 - 257) as u32, 5); // HLIT
    w.push_bits((dist_lengths.len() - 1) as u32, 5); // HDIST
    w.push_bits((18 - 4) as u32, 4); // HCLEN: send 18 entries
    for &ord in CL_ORDER.iter().take(18) {
        w.push_bits(u32::from(cl_lengths[ord]), 3);
    }
    // Lit/len then distance code lengths, no run-length packing.
    for &l in lit_lengths.iter().chain(dist_lengths.iter()) {
        let (c, n) = cl_codes[usize::from(l)];
        w.push_code(c, n);
    }
    for op in ops {
        match op {
            Op::Lit(b) => {
                let (c, n) = lit_codes[usize::from(*b)];
                w.push_code(c, n);
            }
            Op::Match { len, dist } => {
                let (sym, extra, ebits) = length_sym(*len);
                let (c, n) = lit_codes[sym];
                w.push_code(c, n);
                w.push_bits(extra, ebits);
                let (dsym, dextra, dbits) = dist_sym(*dist);
                let (c, n) = dist_codes[dsym];
                w.push_code(c, n);
                w.push_bits(dextra, dbits);
            }
        }
    }
    let (c, n) = lit_codes[256]; // end of block
    w.push_code(c, n);
    w.finish()
}

/// `len` (3..=258) → (lit/len symbol, extra value, extra bit count).
fn length_sym(len: usize) -> (usize, u32, u8) {
    assert!((3..=258).contains(&len));
    for i in (0..29).rev() {
        if len >= usize::from(LENGTH_BASE[i]) {
            return (257 + i, (len - usize::from(LENGTH_BASE[i])) as u32, LENGTH_EXTRA[i]);
        }
    }
    unreachable!()
}

/// `dist` (1..=32768) → (distance symbol, extra value, extra bit count).
fn dist_sym(dist: usize) -> (usize, u32, u8) {
    assert!((1..=32768).contains(&dist));
    for i in (0..30).rev() {
        if dist >= usize::from(DIST_BASE[i]) {
            return (i, (dist - usize::from(DIST_BASE[i])) as u32, DIST_EXTRA[i]);
        }
    }
    unreachable!()
}

/// Literal-only dynamic deflate wrapped as a gzip member.
pub(crate) fn gzip_dynamic(data: &[u8]) -> Vec<u8> {
    let ops: Vec<Op> = data.iter().map(|&b| Op::Lit(b)).collect();
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
    out.extend_from_slice(&deflate_dynamic(&ops));
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

/// Literal-only dynamic deflate wrapped as a zlib stream (`0x78 0x01`
/// header: 32K window, check bits valid, no dictionary).
pub(crate) fn zlib_dynamic(data: &[u8]) -> Vec<u8> {
    let ops: Vec<Op> = data.iter().map(|&b| Op::Lit(b)).collect();
    let mut out = vec![0x78, 0x01];
    out.extend_from_slice(&deflate_dynamic(&ops));
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}
