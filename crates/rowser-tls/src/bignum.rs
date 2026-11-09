//! Minimal big-unsigned-integer arithmetic for RSA and NIST ECC.
//!
//! Limbs are `u64` little-endian, always normalized (no trailing zero
//! limbs, so `Big::zero()` has an empty limb vec). Performance is
//! secondary to simplicity: signature verification runs a handful of
//! times per connection.

use std::cmp::Ordering;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Big {
    /// Little-endian u64 limbs; normalized so the last limb is nonzero.
    pub limbs: Vec<u64>,
}

impl Big {
    pub fn zero() -> Big {
        Big { limbs: Vec::new() }
    }

    pub fn one() -> Big {
        Big { limbs: vec![1] }
    }

    pub fn from_u64(x: u64) -> Big {
        let mut b = Big { limbs: vec![x] };
        b.normalize();
        b
    }

    /// Decode a big-endian byte string.
    pub fn from_be_bytes(bytes: &[u8]) -> Big {
        let mut limbs = Vec::with_capacity((bytes.len() + 7) / 8);
        let mut i = bytes.len();
        while i > 0 {
            let start = i.saturating_sub(8);
            let mut limb = 0u64;
            for &b in &bytes[start..i] {
                limb = (limb << 8) | b as u64;
            }
            limbs.push(limb);
            i = start;
        }
        let mut b = Big { limbs };
        b.normalize();
        b
    }

    /// Encode big-endian, left-padded with zeros to at least `min_len`.
    pub fn to_be_bytes(&self, min_len: usize) -> Vec<u8> {
        let len = min_len.max((self.nbits() + 7) / 8);
        let mut out = vec![0u8; len];
        let mut pos = len;
        for &limb in &self.limbs {
            for i in 0..8 {
                if pos == 0 {
                    break;
                }
                pos -= 1;
                out[pos] = (limb >> (8 * i)) as u8;
            }
        }
        out
    }

    fn normalize(&mut self) {
        while self.limbs.last() == Some(&0) {
            self.limbs.pop();
        }
    }

    pub fn is_zero(&self) -> bool {
        self.limbs.is_empty()
    }

    pub fn nbits(&self) -> usize {
        match self.limbs.last() {
            None => 0,
            Some(&top) => 64 * (self.limbs.len() - 1) + (64 - top.leading_zeros() as usize),
        }
    }

    pub fn bit(&self, i: usize) -> bool {
        let limb = i / 64;
        if limb >= self.limbs.len() {
            return false;
        }
        (self.limbs[limb] >> (i % 64)) & 1 == 1
    }

    pub fn cmp_big(&self, other: &Big) -> Ordering {
        if self.limbs.len() != other.limbs.len() {
            return self.limbs.len().cmp(&other.limbs.len());
        }
        for i in (0..self.limbs.len()).rev() {
            match self.limbs[i].cmp(&other.limbs[i]) {
                Ordering::Equal => continue,
                ord => return ord,
            }
        }
        Ordering::Equal
    }

    /// `self << 1` in place.
    fn shl1(&mut self) {
        let mut carry = 0u64;
        for limb in self.limbs.iter_mut() {
            let next = *limb >> 63;
            *limb = (*limb << 1) | carry;
            carry = next;
        }
        if carry != 0 {
            self.limbs.push(carry);
        }
    }

    fn set_bit0(&mut self) {
        if self.limbs.is_empty() {
            self.limbs.push(1);
        } else {
            self.limbs[0] |= 1;
        }
    }

    /// `self + other` — assumes roughly equal magnitudes (uses max length).
    pub fn add(&self, other: &Big) -> Big {
        let n = self.limbs.len().max(other.limbs.len());
        let mut limbs = Vec::with_capacity(n + 1);
        let mut carry = 0u128;
        for i in 0..n {
            let a = *self.limbs.get(i).unwrap_or(&0) as u128;
            let b = *other.limbs.get(i).unwrap_or(&0) as u128;
            let s = a + b + carry;
            limbs.push(s as u64);
            carry = s >> 64;
        }
        if carry != 0 {
            limbs.push(carry as u64);
        }
        let mut b = Big { limbs };
        b.normalize();
        b
    }

    /// `self - other`; requires `self >= other`.
    pub fn sub(&self, other: &Big) -> Big {
        debug_assert!(self.cmp_big(other) != Ordering::Less);
        let mut limbs = Vec::with_capacity(self.limbs.len());
        let mut borrow = 0u128;
        for i in 0..self.limbs.len() {
            let a = self.limbs[i] as u128;
            let b = *other.limbs.get(i).unwrap_or(&0) as u128;
            // 2^64 + a - b - borrow
            let d = (1u128 << 64) + a - b - borrow;
            limbs.push(d as u64);
            borrow = 1 - (d >> 64); // borrow stays 1 while we underflowed
        }
        debug_assert_eq!(borrow, 0);
        let mut b = Big { limbs };
        b.normalize();
        b
    }

    /// Schoolbook multiply.
    pub fn mul(&self, other: &Big) -> Big {
        if self.is_zero() || other.is_zero() {
            return Big::zero();
        }
        let mut limbs = vec![0u64; self.limbs.len() + other.limbs.len()];
        for (i, &a) in self.limbs.iter().enumerate() {
            let mut carry = 0u128;
            for (j, &b) in other.limbs.iter().enumerate() {
                let t = limbs[i + j] as u128 + a as u128 * b as u128 + carry;
                limbs[i + j] = t as u64;
                carry = t >> 64;
            }
            let mut k = i + other.limbs.len();
            while carry != 0 {
                let t = limbs[k] as u128 + carry;
                limbs[k] = t as u64;
                carry = t >> 64;
                k += 1;
            }
        }
        let mut b = Big { limbs };
        b.normalize();
        b
    }

    /// `self mod m` via binary long division. Simple and obviously correct;
    /// O(nbits * limbs).
    pub fn rem(&self, m: &Big) -> Big {
        debug_assert!(!m.is_zero());
        if self.cmp_big(m) == Ordering::Less {
            return self.clone();
        }
        let mut r = Big::zero();
        for i in (0..self.nbits()).rev() {
            r.shl1();
            if self.bit(i) {
                r.set_bit0();
            }
            if r.cmp_big(m) != Ordering::Less {
                r = r.sub(m);
            }
        }
        r
    }

    /// `(a + b) mod m`, requires a,b < m.
    pub fn addmod(&self, other: &Big, m: &Big) -> Big {
        let s = self.add(other);
        if s.cmp_big(m) != Ordering::Less {
            s.sub(m)
        } else {
            s
        }
    }

    /// `(a - b) mod m`, requires a,b < m.
    pub fn submod(&self, other: &Big, m: &Big) -> Big {
        if self.cmp_big(other) != Ordering::Less {
            self.sub(other)
        } else {
            self.add(m).sub(other)
        }
    }

    /// `(a * b) mod m`.
    pub fn mulmod(&self, other: &Big, m: &Big) -> Big {
        self.mul(other).rem(m)
    }

    /// `self^e mod m`, square-and-multiply MSB-first.
    pub fn powmod(&self, e: &Big, m: &Big) -> Big {
        let mut result = Big::one();
        let base = self.rem(m);
        for i in (0..e.nbits()).rev() {
            result = result.mulmod(&result, m);
            if e.bit(i) {
                result = result.mulmod(&base, m);
            }
        }
        result
    }

    /// `self^-1 mod m` via Fermat: `self^(m-2) mod m`. Requires m prime.
    pub fn invmod(&self, m: &Big) -> Big {
        let two = Big::from_u64(2);
        let e = m.sub(&two);
        self.powmod(&e, m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Big {
        Big::from_be_bytes(
            &(0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
                .collect::<Vec<u8>>(),
        )
    }

    fn to_hex(b: &Big) -> String {
        b.to_be_bytes(0)
            .iter()
            .map(|x| format!("{x:02x}"))
            .collect()
    }

    #[test]
    fn add_sub() {
        let a = hex("ffffffffffffffff");
        let b = hex("01");
        assert_eq!(a.add(&b), hex("010000000000000000"));
        assert_eq!(a.add(&b).sub(&a), b);
    }

    #[test]
    fn mul_div() {
        // (2^32-1)^2 = 2^64 - 2^33 + 1
        let a = hex("ffffffff");
        assert_eq!(to_hex(&a.mul(&a)), "fffffffe00000001");
        // (2^64-1)^2 = 2^128 - 2^65 + 1
        let b = hex("ffffffffffffffff");
        let p = b.mul(&b);
        assert_eq!(to_hex(&p), "fffffffffffffffe0000000000000001");
        assert_eq!(p.rem(&b), Big::zero());
        assert_eq!(p.rem(&hex("02")), Big::one());
    }

    #[test]
    fn powmod_small() {
        // 2^10 mod 1000 = 1024 mod 1000 = 24
        let two = Big::from_u64(2);
        let ten = Big::from_u64(10);
        let m = Big::from_u64(1000);
        assert_eq!(two.powmod(&ten, &m), Big::from_u64(24));
    }

    #[test]
    fn invmod() {
        // 3^-1 mod 97 = 33? 3*33=99≡2 no. 3x≡1 mod97 → x=65 (3*65=195=2*97+1)
        let three = Big::from_u64(3);
        let p = Big::from_u64(97);
        assert_eq!(three.invmod(&p), Big::from_u64(65));
    }

    #[test]
    fn be_roundtrip() {
        let b = hex("c0ffee1234567890");
        assert_eq!(to_hex(&b), "c0ffee1234567890");
        assert_eq!(to_hex(&Big::zero()), "");
        // padding
        assert_eq!(b.to_be_bytes(16).len(), 16);
    }
}
