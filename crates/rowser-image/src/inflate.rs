//! DEFLATE (RFC 1951) and zlib (RFC 1950) decoding, from scratch.

/// Match lengths for length symbols 257..=285 (RFC 1951 §3.2.5).
pub(crate) const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
    131, 163, 195, 227, 258,
];
/// Extra bits for each length symbol.
pub(crate) const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
/// Match distances for distance codes 0..=29.
pub(crate) const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
/// Extra bits for each distance code.
pub(crate) const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12,
    13, 13,
];
/// Order in which code-length-code lengths are transmitted (RFC 1951 §3.2.7).
pub(crate) const CL_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];
const MAXBITS: usize = 15;

/// LSB-first bit reader over a byte slice.
struct Bits<'a> {
    data: &'a [u8],
    /// index of the next unread byte
    pos: usize,
    /// index of the next unread bit within `data[pos]`, 0..8
    bit: u8,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, bit: 0 }
    }

    /// Read `n` bits (n ≤ 16), LSB first.
    fn get(&mut self, n: u8) -> Result<u32, &'static str> {
        let mut v = 0u32;
        for i in 0..n {
            if self.pos >= self.data.len() {
                return Err("deflate: input exhausted");
            }
            v |= (((self.data[self.pos] >> self.bit) & 1) as u32) << i;
            self.bit += 1;
            if self.bit == 8 {
                self.bit = 0;
                self.pos += 1;
            }
        }
        Ok(v)
    }

    /// Skip to the next byte boundary.
    fn align(&mut self) {
        if self.bit != 0 {
            self.bit = 0;
            self.pos += 1;
        }
    }
}

/// Canonical Huffman decoder built from code lengths (puff-style lookup).
struct Huffman {
    /// count[len] = number of symbols with code length `len`
    count: [u16; MAXBITS + 1],
    /// symbols sorted by (code length, symbol value)
    symbol: Vec<u16>,
}

impl Huffman {
    /// Build a decoder from code lengths (0 = symbol not used).
    /// Over-subscribed trees are an error; incomplete trees are allowed
    /// (a decode of a missing code fails at read time).
    fn build(lengths: &[u8]) -> Result<Self, &'static str> {
        let mut count = [0u16; MAXBITS + 1];
        for &l in lengths {
            if l as usize > MAXBITS {
                return Err("deflate: code length > 15");
            }
            count[l as usize] += 1;
        }
        let mut left: i32 = 1;
        for len in 1..=MAXBITS {
            left <<= 1;
            left -= count[len] as i32;
            if left < 0 {
                return Err("deflate: over-subscribed huffman code");
            }
        }
        let used = lengths.len() - count[0] as usize;
        let mut offs = [0usize; MAXBITS + 1];
        for len in 1..MAXBITS {
            offs[len + 1] = offs[len] + count[len] as usize;
        }
        let mut symbol = vec![0u16; used];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbol[offs[l as usize]] = sym as u16;
                offs[l as usize] += 1;
            }
        }
        Ok(Self { count, symbol })
    }

    fn decode(&self, b: &mut Bits) -> Result<u16, &'static str> {
        let mut code: i32 = 0; // bits read so far, MSB-first
        let mut first: i32 = 0; // first canonical code of this length
        let mut index: i32 = 0; // index of first symbol of this length
        for len in 1..=MAXBITS {
            code |= b.get(1)? as i32;
            let count = self.count[len] as i32;
            if code - count < first {
                return Ok(self.symbol[(index + code - first) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err("deflate: invalid huffman code")
    }
}

/// Decode all blocks of a raw DEFLATE stream, appending to `out`.
fn inflate(b: &mut Bits, out: &mut Vec<u8>) -> Result<(), &'static str> {
    loop {
        let last = b.get(1)? != 0;
        match b.get(2)? {
            0 => stored(b, out)?,
            1 => {
                let (lit, dist) = fixed_tables()?;
                compressed(b, out, &lit, &dist)?;
            }
            2 => {
                let (lit, dist) = dynamic_tables(b)?;
                compressed(b, out, &lit, &dist)?;
            }
            _ => return Err("deflate: bad block type"),
        }
        if last {
            return Ok(());
        }
    }
}

/// BTYPE=00: byte-aligned LEN/NLEN plus verbatim bytes.
fn stored(b: &mut Bits, out: &mut Vec<u8>) -> Result<(), &'static str> {
    b.align();
    let len = b.get(16)? as usize;
    let nlen = b.get(16)? as usize;
    if len != (!nlen & 0xFFFF) {
        return Err("deflate: stored LEN/NLEN mismatch");
    }
    for _ in 0..len {
        out.push(b.get(8)? as u8);
    }
    Ok(())
}

/// BTYPE=01: the fixed literal/length and distance tables (RFC 1951 §3.2.6).
fn fixed_tables() -> Result<(Huffman, Huffman), &'static str> {
    let mut lit = [8u8; 288];
    for l in &mut lit[144..256] {
        *l = 9;
    }
    for l in &mut lit[256..280] {
        *l = 7;
    }
    Ok((Huffman::build(&lit)?, Huffman::build(&[5u8; 32])?))
}

/// BTYPE=10: read the dynamic table description (RFC 1951 §3.2.7).
fn dynamic_tables(b: &mut Bits) -> Result<(Huffman, Huffman), &'static str> {
    let hlit = b.get(5)? as usize + 257;
    let hdist = b.get(5)? as usize + 1;
    let hclen = b.get(4)? as usize + 4;
    let mut cl = [0u8; 19];
    for &i in CL_ORDER.iter().take(hclen) {
        cl[i] = b.get(3)? as u8;
    }
    let cl_h = Huffman::build(&cl)?;

    let mut lengths = vec![0u8; hlit + hdist];
    let mut i = 0;
    while i < lengths.len() {
        match cl_h.decode(b)? {
            sym @ 0..=15 => {
                lengths[i] = sym as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return Err("deflate: repeat with no previous length");
                }
                let prev = lengths[i - 1];
                let rep = 3 + b.get(2)? as usize;
                if i + rep > lengths.len() {
                    return Err("deflate: too many code lengths");
                }
                for _ in 0..rep {
                    lengths[i] = prev;
                    i += 1;
                }
            }
            17 => {
                let rep = 3 + b.get(3)? as usize;
                if i + rep > lengths.len() {
                    return Err("deflate: too many code lengths");
                }
                i += rep; // zeros
            }
            _ => {
                let rep = 11 + b.get(7)? as usize;
                if i + rep > lengths.len() {
                    return Err("deflate: too many code lengths");
                }
                i += rep; // zeros
            }
        }
    }
    if lengths[256] == 0 {
        return Err("deflate: no end-of-block code");
    }
    let lit = Huffman::build(&lengths[..hlit])?;
    let dist = Huffman::build(&lengths[hlit..])?;
    Ok((lit, dist))
}

/// Decode one compressed block's symbol stream into `out`.
fn compressed(
    b: &mut Bits,
    out: &mut Vec<u8>,
    lit: &Huffman,
    dist: &Huffman,
) -> Result<(), &'static str> {
    loop {
        let sym = lit.decode(b)? as usize;
        if sym < 256 {
            out.push(sym as u8);
        } else if sym == 256 {
            return Ok(());
        } else {
            let ls = sym - 257;
            if ls >= 29 {
                return Err("deflate: bad length symbol");
            }
            let len = LEN_BASE[ls] as usize + b.get(LEN_EXTRA[ls])? as usize;
            let ds = dist.decode(b)? as usize;
            if ds >= 30 {
                return Err("deflate: bad distance symbol");
            }
            let d = DIST_BASE[ds] as usize + b.get(DIST_EXTRA[ds])? as usize;
            if d == 0 || d > out.len() {
                return Err("deflate: distance too far back");
            }
            let start = out.len() - d;
            for i in 0..len {
                let byte = out[start + i];
                out.push(byte);
            }
        }
    }
}

/// Adler-32 checksum (RFC 1950).
fn adler32(data: &[u8]) -> u32 {
    let mut a = 1u32;
    let mut s = 0u32;
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            s += a;
        }
        a %= 65521;
        s %= 65521;
    }
    (s << 16) | a
}

/// Decode a zlib stream (RFC 1950 header + DEFLATE + adler32 trailer).
/// The adler32 checksum is verified.
pub(crate) fn zlib_decode(data: &[u8]) -> Result<Vec<u8>, &'static str> {
    if data.len() < 2 {
        return Err("zlib: too short");
    }
    let cmf = data[0];
    let flg = data[1];
    if cmf & 0x0F != 8 {
        return Err("zlib: bad compression method");
    }
    if cmf >> 4 > 7 {
        return Err("zlib: window too large");
    }
    if (cmf as u16 * 256 + flg as u16) % 31 != 0 {
        return Err("zlib: bad header check");
    }
    if flg & 0x20 != 0 {
        return Err("zlib: preset dictionary unsupported");
    }
    let mut b = Bits::new(&data[2..]);
    let mut out = Vec::new();
    inflate(&mut b, &mut out)?;
    b.align();
    let pos = 2 + b.pos;
    if pos + 4 > data.len() {
        return Err("zlib: missing adler32");
    }
    let want = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
    if adler32(&out) != want {
        return Err("zlib: adler32 mismatch");
    }
    Ok(out)
}
