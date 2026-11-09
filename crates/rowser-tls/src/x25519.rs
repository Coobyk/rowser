//! X25519 Diffie–Hellman (RFC 7748) over the prime field 2^255 - 19.
//!
//! Field elements are 5 limbs of 51 bits (`Fe = [u64; 5]`, little-endian).
//! Limbs may be "loose" (< 2^54) between operations; multiplication folds
//! everything back below 2^51 and serialization fully contracts.

const MASK51: u64 = (1 << 51) - 1;

type Fe = [u64; 5];

fn fe_add(a: &Fe, b: &Fe) -> Fe {
    [
        a[0] + b[0],
        a[1] + b[1],
        a[2] + b[2],
        a[3] + b[3],
        a[4] + b[4],
    ]
}

/// `a - b`. Normalizes `b` below 2^51, then adds 2p so limbs never
/// underflow; p in this radix is (2^51-19, 2^51-1, ..., 2^51-1).
fn fe_sub(a: &Fe, b: &Fe) -> Fe {
    const B: u64 = 1 << 51;
    let mut bb = *b;
    carry_pass(&mut bb);
    carry_pass(&mut bb);
    carry_pass(&mut bb);
    [
        a[0] + 2 * B - 38 - bb[0],
        a[1] + 2 * B - 2 - bb[1],
        a[2] + 2 * B - 2 - bb[2],
        a[3] + 2 * B - 2 - bb[3],
        a[4] + 2 * B - 2 - bb[4],
    ]
}

/// One carry pass: propagate `h[i] >> 51` into `h[i+1]`, folding the top
/// carry back into limb 0 times 19 (since 2^255 ≡ 19 mod p).
fn carry_pass(h: &mut Fe) {
    for i in 0..5 {
        let c = h[i] >> 51;
        h[i] &= MASK51;
        if i == 4 {
            h[0] += c * 19;
        } else {
            h[i + 1] += c;
        }
    }
}

/// `a * b mod p`. Limbs < 2^54 on input keep every product < 2^113.
fn fe_mul(a: &Fe, b: &Fe) -> Fe {
    let (a0, a1, a2, a3, a4) = (a[0] as u128, a[1] as u128, a[2] as u128, a[3] as u128, a[4] as u128);
    let (b0, b1, b2, b3, b4) = (b[0] as u128, b[1] as u128, b[2] as u128, b[3] as u128, b[4] as u128);
    let b1_19 = b1 * 19;
    let b2_19 = b2 * 19;
    let b3_19 = b3 * 19;
    let b4_19 = b4 * 19;

    let mut h = [0u128; 5];
    h[0] = a0 * b0 + a1 * b4_19 + a2 * b3_19 + a3 * b2_19 + a4 * b1_19;
    h[1] = a0 * b1 + a1 * b0 + a2 * b4_19 + a3 * b3_19 + a4 * b2_19;
    h[2] = a0 * b2 + a1 * b1 + a2 * b0 + a3 * b4_19 + a4 * b3_19;
    h[3] = a0 * b3 + a1 * b2 + a2 * b1 + a3 * b0 + a4 * b4_19;
    h[4] = a0 * b4 + a1 * b3 + a2 * b2 + a3 * b1 + a4 * b0;

    // Carry chain: h[i] -> h[i+1], top carries fold as *19 into h[0], then
    // one more h0 -> h1.
    let mut c = h[0] >> 51;
    h[0] &= MASK51 as u128;
    h[1] += c;
    c = h[1] >> 51;
    h[1] &= MASK51 as u128;
    h[2] += c;
    c = h[2] >> 51;
    h[2] &= MASK51 as u128;
    h[3] += c;
    c = h[3] >> 51;
    h[3] &= MASK51 as u128;
    h[4] += c;
    c = h[4] >> 51;
    h[4] &= MASK51 as u128;
    h[0] += c * 19;
    c = h[0] >> 51;
    h[0] &= MASK51 as u128;
    h[1] += c;

    [
        h[0] as u64,
        h[1] as u64,
        h[2] as u64,
        h[3] as u64,
        h[4] as u64,
    ]
}

fn fe_sq(a: &Fe) -> Fe {
    fe_mul(a, a)
}

fn fe_mul_small(a: &Fe, c: u64) -> Fe {
    let mut h = [0u128; 5];
    for i in 0..5 {
        h[i] = a[i] as u128 * c as u128;
    }
    let mut c = h[0] >> 51;
    h[0] &= MASK51 as u128;
    h[1] += c;
    c = h[1] >> 51;
    h[1] &= MASK51 as u128;
    h[2] += c;
    c = h[2] >> 51;
    h[2] &= MASK51 as u128;
    h[3] += c;
    c = h[3] >> 51;
    h[3] &= MASK51 as u128;
    h[4] += c;
    c = h[4] >> 51;
    h[4] &= MASK51 as u128;
    h[0] += c * 19;
    c = h[0] >> 51;
    h[0] &= MASK51 as u128;
    h[1] += c;
    [
        h[0] as u64,
        h[1] as u64,
        h[2] as u64,
        h[3] as u64,
        h[4] as u64,
    ]
}

/// `f^(p-2) mod p` = f⁻¹, via square-and-multiply over the fixed exponent
/// 2^255 - 21.
fn fe_inv(f: &Fe) -> Fe {
    // p - 2 = 0x7fff…ffeb, little-endian.
    let mut e = [0xffu8; 32];
    e[31] = 0x7f;
    e[0] = 0xeb;
    let mut r: Fe = [1, 0, 0, 0, 0];
    for i in (0..255).rev() {
        r = fe_sq(&r);
        if (e[i / 8] >> (i % 8)) & 1 == 1 {
            r = fe_mul(&r, f);
        }
    }
    r
}

fn fe_from_bytes(s: &[u8; 32]) -> Fe {
    let mut b = *s;
    b[31] &= 0x7f; // ignore the (possibly set) high bit per RFC 7748
    let l0 = u64::from_le_bytes(b[0..8].try_into().unwrap());
    let l1 = u64::from_le_bytes(b[8..16].try_into().unwrap());
    let l2 = u64::from_le_bytes(b[16..24].try_into().unwrap());
    let l3 = u64::from_le_bytes(b[24..32].try_into().unwrap());
    [
        l0 & MASK51,
        ((l0 >> 51) | (l1 << 13)) & MASK51,
        ((l1 >> 38) | (l2 << 26)) & MASK51,
        ((l2 >> 25) | (l3 << 39)) & MASK51,
        l3 >> 12,
    ]
}

fn fe_to_bytes(f: &Fe) -> [u8; 32] {
    let mut h = *f;
    carry_pass(&mut h);
    carry_pass(&mut h);
    carry_pass(&mut h);
    // Now limbs < 2^51, so h < 2^255 but may still be >= p.
    // q = (h + 19) >> 255: 1 iff h >= p. The top carry must NOT fold back
    // as *19 — it is exactly q.
    let mut t = h;
    t[0] += 19;
    for i in 0..4 {
        let c = t[i] >> 51;
        t[i] &= MASK51;
        t[i + 1] += c;
    }
    let q = t[4] >> 51;
    let r = if q == 1 {
        // h - p = (h + 19) - 2^255: clear the overflow bit and keep t.
        t[4] &= MASK51;
        t
    } else {
        h
    };
    // Limbs are canonical now; pack 5x51 into 4x64.
    let l0 = r[0] | (r[1] << 51);
    let l1 = (r[1] >> 13) | (r[2] << 38);
    let l2 = (r[2] >> 26) | (r[3] << 25);
    let l3 = (r[3] >> 39) | (r[4] << 12);
    let mut out = [0u8; 32];
    out[0..8].copy_from_slice(&l0.to_le_bytes());
    out[8..16].copy_from_slice(&l1.to_le_bytes());
    out[16..24].copy_from_slice(&l2.to_le_bytes());
    out[24..32].copy_from_slice(&l3.to_le_bytes());
    out
}

fn cswap(swap: u64, a: &mut Fe, b: &mut Fe) {
    let m = 0u64.wrapping_sub(swap);
    for i in 0..5 {
        let t = m & (a[i] ^ b[i]);
        a[i] ^= t;
        b[i] ^= t;
    }
}

/// `X25519(k, u)` — RFC 7748 §5 Montgomery ladder.
pub fn x25519(scalar: &[u8; 32], point: &[u8; 32]) -> [u8; 32] {
    let mut k = *scalar;
    k[0] &= 248;
    k[31] &= 127;
    k[31] |= 64;

    let x1 = fe_from_bytes(point);
    let mut x2: Fe = [1, 0, 0, 0, 0];
    let mut z2: Fe = [0, 0, 0, 0, 0];
    let mut x3 = x1;
    let mut z3: Fe = [1, 0, 0, 0, 0];
    let mut swap = 0u64;

    for t in (0..255).rev() {
        let kt = ((k[t / 8] >> (t % 8)) & 1) as u64;
        swap ^= kt;
        cswap(swap, &mut x2, &mut x3);
        cswap(swap, &mut z2, &mut z3);
        swap = kt;

        let a = fe_add(&x2, &z2);
        let aa = fe_sq(&a);
        let b = fe_sub(&x2, &z2);
        let bb = fe_sq(&b);
        let e = fe_sub(&aa, &bb);
        let c = fe_add(&x3, &z3);
        let d = fe_sub(&x3, &z3);
        let da = fe_mul(&d, &a);
        let cb = fe_mul(&c, &b);
        let da_plus_cb = fe_add(&da, &cb);
        let da_minus_cb = fe_sub(&da, &cb);
        x3 = fe_sq(&da_plus_cb);
        z3 = fe_mul(&x1, &fe_sq(&da_minus_cb));
        x2 = fe_mul(&aa, &bb);
        z2 = fe_mul(&e, &fe_add(&aa, &fe_mul_small(&e, 121665)));
    }
    cswap(swap, &mut x2, &mut x3);
    cswap(swap, &mut z2, &mut z3);

    let z2_inv = fe_inv(&z2);
    fe_to_bytes(&fe_mul(&x2, &z2_inv))
}

/// An X25519 private key (raw 32 bytes; clamping happens inside `x25519`).
#[derive(Clone)]
pub struct PrivateKey(pub [u8; 32]);

/// An X25519 public key.
#[derive(Clone, Copy)]
pub struct PublicKey(pub [u8; 32]);

impl PrivateKey {
    pub fn generate() -> PrivateKey {
        let mut k = [0u8; 32];
        crate::rand_bytes(&mut k);
        PrivateKey(k)
    }

    pub fn public_key(&self) -> PublicKey {
        // The X25519 base point is u = 9, little-endian: 0x09 followed by zeros.
        let mut basepoint = [0u8; 32];
        basepoint[0] = 9;
        PublicKey(x25519(&self.0, &basepoint))
    }

    /// Diffie–Hellman against a peer public key.
    pub fn dh(&self, peer: &PublicKey) -> [u8; 32] {
        x25519(&self.0, &peer.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex32(s: &str) -> [u8; 32] {
        let v: Vec<u8> = (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect();
        v.try_into().unwrap()
    }

    fn hexify(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn rfc7748_vector_1() {
        let out = x25519(
            &hex32("a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4"),
            &hex32("e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c"),
        );
        assert_eq!(
            hexify(&out),
            "c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552"
        );
    }

    #[test]
    fn rfc7748_vector_2() {
        let out = x25519(
            &hex32("4b66e9d4d1b4673c5ad22691957d6af5c11b6421e0ea01d42ca4169e7918ba0d"),
            &hex32("e5210f12786811d3f4b7959d0538ae2c31dbe7106fc03c3efc4cd549c715a493"),
        );
        assert_eq!(
            hexify(&out),
            "95cbde9476e8907d7aade45cb4b873f88b595a68799fa152e6f8f7647aac7957"
        );
    }

    #[test]
    fn rfc7748_iterated() {
        // One iteration of the RFC 7748 iterated test: k = u = 09 00..00.
        let nine = hex32("0900000000000000000000000000000000000000000000000000000000000000");
        let out = x25519(&nine, &nine);
        assert_eq!(
            hexify(&out),
            "422c8e7a6227d7bca1350b3e2bb7279f7897b87bb6854b783c60e80311ae3079"
        );
    }

    #[test]
    fn rfc7748_dh() {
        // Alice/Bob key agreement from RFC 7748 §6.1.
        let alice_priv = PrivateKey(hex32(
            "77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a",
        ));
        let bob_priv = PrivateKey(hex32(
            "5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb",
        ));
        assert_eq!(
            hexify(&alice_priv.public_key().0),
            "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a"
        );
        assert_eq!(
            hexify(&bob_priv.public_key().0),
            "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f"
        );
        let shared = "4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742";
        assert_eq!(hexify(&alice_priv.dh(&bob_priv.public_key())), shared);
        assert_eq!(hexify(&bob_priv.dh(&alice_priv.public_key())), shared);
    }
}
