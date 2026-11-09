//! Minimal DER (X.690) reader for X.509 parsing.
//!
//! Only what certificate parsing needs: definite lengths, tag reading,
//! and helpers to descend into constructed values.

use crate::{Error, Result};

/// One parsed TLV. `value` is the contents octets; `raw` is the whole
/// TLV including tag+length (needed to hash TBSCertificate).
#[derive(Clone, Copy)]
pub struct Tlv<'a> {
    pub tag: u8,
    pub value: &'a [u8],
    pub raw: &'a [u8],
}

pub const TAG_INTEGER: u8 = 0x02;
pub const TAG_BIT_STRING: u8 = 0x03;
pub const TAG_OCTET_STRING: u8 = 0x04;
pub const TAG_NULL: u8 = 0x05;
pub const TAG_OID: u8 = 0x06;
pub const TAG_UTF8_STRING: u8 = 0x0c;
pub const TAG_SEQUENCE: u8 = 0x30;
pub const TAG_SET: u8 = 0x31;
pub const TAG_PRINTABLE_STRING: u8 = 0x13;
pub const TAG_TELETEX_STRING: u8 = 0x14;
pub const TAG_IA5_STRING: u8 = 0x16;
pub const TAG_UTC_TIME: u8 = 0x17;
pub const TAG_GENERALIZED_TIME: u8 = 0x18;
pub const TAG_BMP_STRING: u8 = 0x1e;
pub const TAG_BOOLEAN: u8 = 0x01;

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf, pos: 0 }
    }

    pub fn at_end(&self) -> bool {
        self.pos >= self.buf.len()
    }

    /// Read the next TLV.
    pub fn read(&mut self) -> Result<Tlv<'a>> {
        if self.pos >= self.buf.len() {
            return Err(Error::Decode("der: unexpected end"));
        }
        let start = self.pos;
        let tag = self.buf[self.pos];
        self.pos += 1;
        // Multi-byte (high) tag numbers are rejected: nothing we parse uses them.
        if tag & 0x1f == 0x1f {
            return Err(Error::Decode("der: high tag number unsupported"));
        }
        if self.pos >= self.buf.len() {
            return Err(Error::Decode("der: truncated length"));
        }
        let first_len = self.buf[self.pos];
        self.pos += 1;
        let len: usize = if first_len < 0x80 {
            first_len as usize
        } else {
            let n = (first_len & 0x7f) as usize;
            if n == 0 || n > 4 || self.pos + n > self.buf.len() {
                return Err(Error::Decode("der: bad long-form length"));
            }
            let mut l = 0usize;
            for _ in 0..n {
                l = (l << 8) | self.buf[self.pos] as usize;
                self.pos += 1;
            }
            l
        };
        if self.pos + len > self.buf.len() {
            return Err(Error::Decode("der: value overruns buffer"));
        }
        let value = &self.buf[self.pos..self.pos + len];
        let raw = &self.buf[start..self.pos + len];
        self.pos += len;
        Ok(Tlv { tag, value, raw })
    }

    /// Read a TLV and require an exact tag byte.
    pub fn expect(&mut self, tag: u8) -> Result<Tlv<'a>> {
        let tlv = self.read()?;
        if tlv.tag != tag {
            return Err(Error::Decode("der: unexpected tag"));
        }
        Ok(tlv)
    }
}

/// Decode an OID's contents octets to dotted form components.
/// Returns the raw bytes — we compare OIDs byte-for-byte against constants.
pub fn oid_eq(value: &[u8], dotted: &[u64]) -> bool {
    // Encode `dotted` as DER OID contents and compare.
    let mut enc = Vec::new();
    if dotted.len() < 2 {
        return false;
    }
    enc.push((dotted[0] * 40 + dotted[1]) as u8);
    for &arc in &dotted[2..] {
        // base-128, most significant group first, high bit = continuation
        let mut groups = [0u8; 10];
        let mut n = 0;
        let mut v = arc;
        loop {
            groups[n] = (v & 0x7f) as u8;
            v >>= 7;
            n += 1;
            if v == 0 {
                break;
            }
        }
        for i in (0..n).rev() {
            let cont = if i == 0 { 0 } else { 0x80 };
            enc.push(groups[i] | cont);
        }
    }
    enc.as_slice() == value
}
