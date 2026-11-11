//! DEFLATE (RFC 1951) decompression with zlib (RFC 1950) and gzip (RFC 1952)
//! wrappers, written from scratch: LSB-first bit reader, fixed and dynamic
//! canonical Huffman tables, and LZ77 back-references into the output window.

use std::fmt;

/// Ways a compressed stream can be malformed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InflateError {
    /// Input ended before the compressed stream did.
    UnexpectedEof,
    /// Structurally invalid data: bad block type, bad Huffman code,
    /// inconsistent lengths.
    InvalidData(&'static str),
    /// Bad magic, method, or header check in a zlib/gzip wrapper.
    BadHeader(&'static str),
    /// Trailer checksum (gzip CRC32/ISIZE, zlib Adler-32) does not match
    /// the decompressed payload.
    ChecksumMismatch,
}

impl fmt::Display for InflateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof => write!(f, "unexpected end of compressed input"),
            Self::InvalidData(m) => write!(f, "invalid deflate stream: {m}"),
            Self::BadHeader(m) => write!(f, "bad compressed header: {m}"),
            Self::ChecksumMismatch => write!(f, "compressed data checksum mismatch"),
        }
    }
}

impl std::error::Error for InflateError {}

/// Length symbol bases/extra bits (RFC 1951 §3.2.5), indexed by symbol - 257.
pub const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
pub const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
/// Distance symbol bases/extra bits, indexed by distance symbol.
pub const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
pub const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
/// Order in which code-length code lengths are transmitted (RFC 1951 §3.2.7).
const CL_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

const MAX_BITS: usize = 15;

/// LSB-first bit reader: DEFLATE packs bits starting at the low end of each
/// byte, while Huffman codes are stored most-significant-bit first.
struct BitReader<'a> {
    data: &'a [u8],
    byte: usize,
    /// Next bit index within `data[byte]`, 0..8.
    bit: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte: 0,
            bit: 0,
        }
    }

    fn bit(&mut self) -> Result<u32, InflateError> {
        if self.byte >= self.data.len() {
            return Err(InflateError::UnexpectedEof);
        }
        let b = u32::from((self.data[self.byte] >> self.bit) & 1);
        self.bit += 1;
        if self.bit == 8 {
            self.bit = 0;
            self.byte += 1;
        }
        Ok(b)
    }

    /// Read `n` bits, first-read bit in the least-significant position.
    fn bits(&mut self, n: u8) -> Result<u32, InflateError> {
        let mut v = 0;
        for i in 0..n {
            v |= self.bit()? << i;
        }
        Ok(v)
    }

    fn byte_align(&mut self) {
        if self.bit != 0 {
            self.bit = 0;
            self.byte += 1;
        }
    }

    fn take_bytes(&mut self, n: usize) -> Result<&'a [u8], InflateError> {
        if self.bit != 0 {
            return Err(InflateError::InvalidData("byte read while unaligned"));
        }
        if self.byte + n > self.data.len() {
            return Err(InflateError::UnexpectedEof);
        }
        let s = &self.data[self.byte..self.byte + n];
        self.byte += n;
        Ok(s)
    }

    fn u16le(&mut self) -> Result<u16, InflateError> {
        let b = self.take_bytes(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32le(&mut self) -> Result<u32, InflateError> {
        let b = self.take_bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u32be(&mut self) -> Result<u32, InflateError> {
        let b = self.take_bytes(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
}

/// Canonical Huffman decoding via per-length symbol counts (the `puff`
/// approach): decode walks one bit at a time, no lookup tables needed.
struct Huffman {
    /// counts[len] = number of symbols with that code length.
    counts: [u16; MAX_BITS + 1],
    /// Symbols sorted by (code length, symbol value).
    symbols: Vec<u16>,
}

impl Huffman {
    fn build(lengths: &[u8]) -> Result<Self, InflateError> {
        let mut counts = [0u16; MAX_BITS + 1];
        for &l in lengths {
            if l as usize > MAX_BITS {
                return Err(InflateError::InvalidData("code length over 15"));
            }
            counts[l as usize] += 1;
        }
        counts[0] = 0;

        // Over-subscribed codes are rejected; incomplete codes are allowed
        // (decoding an unused pattern simply errors at read time).
        let mut left: i32 = 1;
        for &c in counts.iter().skip(1) {
            left <<= 1;
            left -= i32::from(c);
            if left < 0 {
                return Err(InflateError::InvalidData("over-subscribed huffman code"));
            }
        }

        let mut offs = [0u16; MAX_BITS + 1];
        for l in 1..MAX_BITS {
            offs[l + 1] = offs[l] + counts[l];
        }
        let mut symbols = vec![0u16; lengths.iter().filter(|&&l| l != 0).count()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offs[l as usize] as usize] = sym as u16;
                offs[l as usize] += 1;
            }
        }
        Ok(Self { counts, symbols })
    }

    fn decode(&self, r: &mut BitReader) -> Result<u16, InflateError> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..=MAX_BITS {
            code |= r.bit()? as i32;
            let count = i32::from(self.counts[len]);
            if code - first < count {
                return Ok(self.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(InflateError::InvalidData("invalid huffman code"))
    }
}

/// Decompress a raw DEFLATE stream (RFC 1951). Trailing bytes after the
/// final block are ignored.
pub fn inflate(data: &[u8]) -> Result<Vec<u8>, InflateError> {
    inflate_to(&mut BitReader::new(data))
}

/// Decompress a zlib stream (RFC 1950): 2-byte header, DEFLATE body,
/// big-endian Adler-32 trailer.
pub fn inflate_zlib(data: &[u8]) -> Result<Vec<u8>, InflateError> {
    if data.len() < 2 {
        return Err(InflateError::UnexpectedEof);
    }
    let (cmf, flg) = (data[0], data[1]);
    if cmf & 0x0f != 8 {
        return Err(InflateError::BadHeader("zlib method is not deflate"));
    }
    if cmf >> 4 > 7 {
        return Err(InflateError::BadHeader("zlib window larger than 32k"));
    }
    if (u16::from(cmf) * 256 + u16::from(flg)) % 31 != 0 {
        return Err(InflateError::BadHeader("zlib header check failed"));
    }
    if flg & 0x20 != 0 {
        return Err(InflateError::BadHeader(
            "zlib preset dictionary unsupported",
        ));
    }
    let mut r = BitReader::new(&data[2..]);
    let out = inflate_to(&mut r)?;
    r.byte_align();
    if r.u32be()? != adler32(&out) {
        return Err(InflateError::ChecksumMismatch);
    }
    Ok(out)
}

/// Decompress a gzip stream (RFC 1952): header with optional fields,
/// DEFLATE body, little-endian CRC32 + ISIZE trailer.
pub fn inflate_gzip(data: &[u8]) -> Result<Vec<u8>, InflateError> {
    if data.len() < 10 {
        return Err(InflateError::UnexpectedEof);
    }
    if data[0] != 0x1f || data[1] != 0x8b {
        return Err(InflateError::BadHeader("bad gzip magic"));
    }
    if data[2] != 8 {
        return Err(InflateError::BadHeader("gzip member is not deflate"));
    }
    let flg = data[3];
    if flg & 0xe0 != 0 {
        return Err(InflateError::BadHeader("reserved gzip flag bits set"));
    }
    // Skip MTIME(4), XFL, OS — already covered by the 10-byte minimum.
    let mut pos = 10;
    if flg & 0x04 != 0 {
        // FEXTRA: 2-byte little-endian length + payload.
        if pos + 2 > data.len() {
            return Err(InflateError::UnexpectedEof);
        }
        let xlen = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2 + xlen;
        if pos > data.len() {
            return Err(InflateError::UnexpectedEof);
        }
    }
    for flag in [0x08u8, 0x10] {
        // FNAME, FCOMMENT: NUL-terminated strings.
        if flg & flag != 0 {
            match data[pos..].iter().position(|&b| b == 0) {
                Some(i) => pos += i + 1,
                None => return Err(InflateError::UnexpectedEof),
            }
        }
    }
    if flg & 0x02 != 0 {
        pos += 2; // FHCRC: header CRC16, not verified.
    }
    if pos > data.len() {
        return Err(InflateError::UnexpectedEof);
    }
    let mut r = BitReader {
        data,
        byte: pos,
        bit: 0,
    };
    let out = inflate_to(&mut r)?;
    r.byte_align();
    let crc = r.u32le()?;
    let isize = r.u32le()?;
    if crc != crc32(&out) || isize != out.len() as u32 {
        return Err(InflateError::ChecksumMismatch);
    }
    Ok(out)
}

/// Decode blocks until BFINAL; the reader is left just past the last block.
fn inflate_to(r: &mut BitReader) -> Result<Vec<u8>, InflateError> {
    let mut out = Vec::new();
    loop {
        let bfinal = r.bit()?;
        match r.bits(2)? {
            0 => {
                r.byte_align();
                let len = r.u16le()?;
                let nlen = r.u16le()?;
                if len != !nlen {
                    return Err(InflateError::InvalidData("stored LEN/NLEN mismatch"));
                }
                out.extend_from_slice(r.take_bytes(usize::from(len))?);
            }
            1 => {
                let (lit, dist) = fixed_tables()?;
                inflate_block(r, &lit, &dist, &mut out)?;
            }
            2 => {
                let (lit, dist) = dynamic_tables(r)?;
                inflate_block(r, &lit, &dist, &mut out)?;
            }
            _ => return Err(InflateError::InvalidData("reserved block type")),
        }
        if bfinal == 1 {
            return Ok(out);
        }
    }
}

/// The fixed Huffman tables of RFC 1951 §3.2.6.
fn fixed_tables() -> Result<(Huffman, Huffman), InflateError> {
    let mut lit_lengths = [0u8; 288];
    for (i, l) in lit_lengths.iter_mut().enumerate() {
        *l = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    // 32 five-bit distance codes; 30 and 31 exist but are invalid to use.
    Ok((Huffman::build(&lit_lengths)?, Huffman::build(&[5; 32])?))
}

/// Read a dynamic block header (RFC 1951 §3.2.7) and build its tables.
fn dynamic_tables(r: &mut BitReader) -> Result<(Huffman, Huffman), InflateError> {
    let hlit = r.bits(5)? as usize + 257;
    let hdist = r.bits(5)? as usize + 1;
    let hclen = r.bits(4)? as usize + 4;
    if hlit > 286 || hdist > 32 {
        return Err(InflateError::InvalidData(
            "too many length/distance symbols",
        ));
    }

    let mut cl_lengths = [0u8; 19];
    for &ord in CL_ORDER.iter().take(hclen) {
        cl_lengths[ord] = r.bits(3)? as u8;
    }
    let cl = Huffman::build(&cl_lengths)?;

    let total = hlit + hdist;
    let mut lengths = vec![0u8; total];
    let mut i = 0;
    while i < total {
        match cl.decode(r)? {
            l @ 0..=15 => {
                lengths[i] = l as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return Err(InflateError::InvalidData("repeat with no previous length"));
                }
                let prev = lengths[i - 1];
                let rep = 3 + r.bits(2)? as usize;
                if i + rep > total {
                    return Err(InflateError::InvalidData("length repeat overflows"));
                }
                for _ in 0..rep {
                    lengths[i] = prev;
                    i += 1;
                }
            }
            rep_sym @ (17 | 18) => {
                let rep = if rep_sym == 17 {
                    3 + r.bits(3)? as usize
                } else {
                    11 + r.bits(7)? as usize
                };
                if i + rep > total {
                    return Err(InflateError::InvalidData("zero repeat overflows"));
                }
                i += rep; // already zeroed
            }
            _ => return Err(InflateError::InvalidData("bad code-length symbol")),
        }
    }
    if lengths[256] == 0 {
        return Err(InflateError::InvalidData("missing end-of-block code"));
    }
    Ok((
        Huffman::build(&lengths[..hlit])?,
        Huffman::build(&lengths[hlit..])?,
    ))
}

/// Decode one block's literal/match symbols into `out`.
fn inflate_block(
    r: &mut BitReader,
    lit: &Huffman,
    dist: &Huffman,
    out: &mut Vec<u8>,
) -> Result<(), InflateError> {
    loop {
        match lit.decode(r)? {
            b @ 0..=255 => out.push(b as u8),
            256 => return Ok(()),
            sym @ 257..=285 => {
                let i = usize::from(sym - 257);
                let len = LENGTH_BASE[i] as usize + r.bits(LENGTH_EXTRA[i])? as usize;
                let dsym = usize::from(dist.decode(r)?);
                if dsym >= 30 {
                    return Err(InflateError::InvalidData("bad distance symbol"));
                }
                let d = DIST_BASE[dsym] as usize + r.bits(DIST_EXTRA[dsym])? as usize;
                if d > out.len() {
                    return Err(InflateError::InvalidData("distance too far back"));
                }
                // Byte-wise copy handles the overlapping len > dist case.
                let start = out.len() - d;
                for k in 0..len {
                    let b = out[start + k];
                    out.push(b);
                }
            }
            _ => return Err(InflateError::InvalidData("bad literal/length symbol")),
        }
    }
}

/// Reflected CRC32 (polynomial 0xEDB88320), bitwise — tables are only a
/// speed optimisation we don't need yet.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

pub(crate) fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + u32::from(x)) % MOD;
        b = (b + a) % MOD;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{self, Op};

    #[test]
    fn stored_block() {
        // BFINAL=1 BTYPE=00, LEN=5, NLEN=!5, "hello".
        let data = [0x01, 0x05, 0x00, 0xfa, 0xff, b'h', b'e', b'l', b'l', b'o'];
        assert_eq!(inflate(&data).unwrap(), b"hello");
    }

    #[test]
    fn stored_blocks_multiple() {
        let data = [
            0x00, 0x02, 0x00, 0xfd, 0xff, b'h', b'e', // non-final "he"
            0x01, 0x03, 0x00, 0xfc, 0xff, b'l', b'l', b'o', // final "llo"
        ];
        assert_eq!(inflate(&data).unwrap(), b"hello");
    }

    #[test]
    fn fixed_huffman_literals() {
        let d = testutil::deflate_fixed(b"Hello, Rowser! ~123");
        assert_eq!(inflate(&d).unwrap(), b"Hello, Rowser! ~123");
    }

    #[test]
    fn fixed_huffman_match() {
        // Literal 'a', then length-4 distance-1 back-reference → "aaaaa".
        // (len > dist exercises the byte-wise overlapping copy.)
        let mut w = testutil::BitWriter::new();
        w.push_bits(1, 1); // BFINAL
        w.push_bits(1, 2); // BTYPE=01 fixed
        let (c, n) = testutil::fixed_lit(usize::from(b'a'));
        w.push_code(c, n);
        let (c, n) = testutil::fixed_lit(258); // length 4
        w.push_code(c, n);
        w.push_code(0, 5); // distance symbol 0 → distance 1
        let (c, n) = testutil::fixed_lit(256); // end of block
        w.push_code(c, n);
        assert_eq!(inflate(&w.finish()).unwrap(), b"aaaaa");
    }

    #[test]
    fn dynamic_huffman_literals() {
        let payload = b"the quick brown fox jumps over the lazy dog. \
                        the quick brown fox jumps over the lazy dog.";
        let ops: Vec<Op> = payload.iter().map(|&b| Op::Lit(b)).collect();
        let d = testutil::deflate_dynamic(&ops);
        assert_eq!(inflate(&d).unwrap(), payload);
    }

    #[test]
    fn dynamic_huffman_match() {
        let d = testutil::deflate_dynamic(&[
            Op::Lit(b'a'),
            Op::Lit(b'b'),
            Op::Match { len: 5, dist: 2 },
            Op::Lit(b'!'),
        ]);
        assert_eq!(inflate(&d).unwrap(), b"abababa!");
    }

    #[test]
    fn dynamic_huffman_length_extra_bits() {
        // Length 200 needs extra bits (symbol 283, base 195, 5 extra bits).
        let d = testutil::deflate_dynamic(&[Op::Lit(b'x'), Op::Match { len: 200, dist: 1 }]);
        assert_eq!(inflate(&d).unwrap(), vec![b'x'; 201]);
    }

    #[test]
    fn zlib_roundtrip() {
        let payload = b"zlib-wrapped deflate payload 0123456789";
        assert_eq!(
            inflate_zlib(&testutil::zlib_dynamic(payload)).unwrap(),
            payload
        );
    }

    #[test]
    fn gzip_roundtrip() {
        let payload = b"gzip-wrapped deflate payload, hello hello hello";
        assert_eq!(
            inflate_gzip(&testutil::gzip_dynamic(payload)).unwrap(),
            payload
        );
    }

    #[test]
    fn gzip_stored_member() {
        // Hand-assembled gzip member carrying a stored (uncompressed) block.
        let payload = b"stored hello";
        let mut d = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
        d.push(0x01); // BFINAL, stored
        d.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        d.extend_from_slice(&(!(payload.len() as u16)).to_le_bytes());
        d.extend_from_slice(payload);
        d.extend_from_slice(&crc32(payload).to_le_bytes());
        d.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        assert_eq!(inflate_gzip(&d).unwrap(), payload);
    }

    #[test]
    fn gzip_with_filename_header() {
        // FNAME flag set: "file.txt\0" between header and deflate body.
        // Payload: 'n' + match len 11 dist 1 → twelve n's.
        let expect = vec![b'n'; 12];
        let deflate = testutil::deflate_dynamic(&[Op::Lit(b'n'), Op::Match { len: 11, dist: 1 }]);
        let mut d = vec![0x1f, 0x8b, 8, 0x08, 0, 0, 0, 0, 0, 0xff];
        d.extend_from_slice(b"file.txt\0");
        d.extend_from_slice(&deflate);
        d.extend_from_slice(&crc32(&expect).to_le_bytes());
        d.extend_from_slice(&(expect.len() as u32).to_le_bytes());
        assert_eq!(inflate_gzip(&d).unwrap(), expect);
    }

    #[test]
    fn corrupt_gzip_crc_fails() {
        let mut d = testutil::gzip_dynamic(b"abc");
        let n = d.len();
        d[n - 5] ^= 0xff; // last byte of the CRC32 field
        assert_eq!(inflate_gzip(&d), Err(InflateError::ChecksumMismatch));
    }

    #[test]
    fn corrupt_zlib_adler_fails() {
        let mut d = testutil::zlib_dynamic(b"abc");
        let n = d.len();
        d[n - 1] ^= 0xff;
        assert_eq!(inflate_zlib(&d), Err(InflateError::ChecksumMismatch));
    }

    #[test]
    fn bad_inputs() {
        assert!(matches!(
            inflate(&[0x07]),
            Err(InflateError::InvalidData(_))
        )); // BTYPE=3
        assert_eq!(inflate(&[0x01]), Err(InflateError::UnexpectedEof));
        assert!(matches!(
            inflate_gzip(b"not gzip at all....."),
            Err(InflateError::BadHeader(_))
        ));
        assert!(matches!(
            inflate_zlib(&[0x78, 0x02, 0x01]),
            Err(InflateError::BadHeader(_))
        ));
    }
}

#[doc(hidden)]
pub mod testutil;

#[cfg(test)]
mod wiki_probe {
    #[test]
    fn inflate_real_gzip() {
        let gz = std::fs::read("/tmp/wiki.html").unwrap();
        match crate::inflate_gzip(&gz) {
            Ok(body) => eprintln!(
                "INFLATED {} bytes, head={:?}",
                body.len(),
                String::from_utf8_lossy(&body[..body.len().min(80)])
            ),
            Err(e) => eprintln!("INFLATE ERR {e:?}"),
        }
    }
}
