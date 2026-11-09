//! ECDSA verification over the NIST prime curves P-256 and P-384.
//!
//! Point arithmetic uses Jacobian coordinates (a = -3 curves) so scalar
//! multiplication needs a single field inversion at the end. Built on the
//! generic [`Big`] arithmetic — verify-only, so a few hundred milliseconds
//! worst case is acceptable.

use crate::bignum::Big;
use crate::{Error, Result};

/// A NIST prime curve: field prime, group order, generator.
pub struct Curve {
    pub p: Big,
    pub n: Big,
    pub gx: Big,
    pub gy: Big,
    /// Coordinate byte length (32 or 48).
    pub coord_len: usize,
}

fn big(hex: &str) -> Big {
    Big::from_be_bytes(
        &(0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect::<Vec<u8>>(),
    )
}

/// secp256r1 / prime256v1 (OID 1.2.840.10045.3.1.7).
pub fn p256() -> Curve {
    Curve {
        p: big("ffffffff00000001000000000000000000000000ffffffffffffffffffffffff"),
        n: big("ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551"),
        gx: big("6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296"),
        gy: big("4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5"),
        coord_len: 32,
    }
}

/// secp384r1 (OID 1.3.132.0.34).
pub fn p384() -> Curve {
    Curve {
        p: big("fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe\
                ffffffff0000000000000000ffffffff"),
        n: big("ffffffffffffffffffffffffffffffffffffffffffffffffc7634d81f4372ddf\
                581a0db248b0a77aecec196accc52973"),
        gx: big("aa87ca22be8b05378eb1c71ef320ad746e1d3b628ba79b9859f741e082542a38\
                5502f25dbf55296c3a545e3872760ab7"),
        gy: big("3617de4a96262c6f5d9e98bf9292dc29f8f41dbd289a147ce9da3113b5f0b8c0\
                0a60b1ce1d7e819d7a431d7c90ea0e5f"),
        coord_len: 48,
    }
}

/// Jacobian point; `z == 0` means the point at infinity.
#[derive(Clone)]
struct Pt {
    x: Big,
    y: Big,
    z: Big,
}

impl Pt {
    fn infinity() -> Pt {
        Pt {
            x: Big::one(),
            y: Big::one(),
            z: Big::zero(),
        }
    }

    fn affine(x: Big, y: Big) -> Pt {
        Pt {
            x,
            y,
            z: Big::one(),
        }
    }

    fn is_infinity(&self) -> bool {
        self.z.is_zero()
    }
}

fn mulmod(a: &Big, b: &Big, m: &Big) -> Big {
    a.mulmod(b, m)
}

fn sqmod(a: &Big, m: &Big) -> Big {
    a.mulmod(a, m)
}

/// Point doubling, Jacobian, a = -3 (dbl-2001-b from the EFD).
fn double(pt: &Pt, p: &Big) -> Pt {
    if pt.is_infinity() || pt.y.is_zero() {
        return Pt::infinity();
    }
    let delta = sqmod(&pt.z, p); // Z1^2
    let gamma = sqmod(&pt.y, p); // Y1^2
    let beta = mulmod(&pt.x, &gamma, p); // X1*gamma
    // alpha = 3*(X1-delta)*(X1+delta)
    let alpha = mulmod(
        &Big::from_u64(3),
        &mulmod(&pt.x.submod(&delta, p), &pt.x.addmod(&delta, p), p),
        p,
    );
    let four_beta = beta.addmod(&beta, p).addmod(&beta.addmod(&beta, p), p);
    let eight_beta = four_beta.addmod(&four_beta, p);
    // X3 = alpha^2 - 8*beta
    let x3 = sqmod(&alpha, p).submod(&eight_beta, p);
    // Z3 = (Y1+Z1)^2 - gamma - delta
    let z3 = pt
        .y
        .addmod(&pt.z, p)
        .mulmod(&pt.y.addmod(&pt.z, p), p)
        .submod(&gamma, p)
        .submod(&delta, p);
    // Y3 = alpha*(4*beta - X3) - 8*gamma^2
    let eight_gamma2 = {
        let g2 = sqmod(&gamma, p);
        let g4 = g2.addmod(&g2, p).addmod(&g2.addmod(&g2, p), p); // 4*g2
        g4.addmod(&g4, p) // 8*g2
    };
    let y3 = mulmod(&alpha, &four_beta.submod(&x3, p), p).submod(&eight_gamma2, p);
    Pt { x: x3, y: y3, z: z3 }
}

/// Mixed addition: Jacobian `a` + affine `b` (z2 = 1), madd-2007-bl style
/// but returning the un-doubled variant (R = S2 - Y1, no factor 2).
fn add_mixed(a: &Pt, b: &Pt, p: &Big) -> Pt {
    if a.is_infinity() {
        return b.clone();
    }
    if b.is_infinity() {
        return a.clone();
    }
    let z1z1 = sqmod(&a.z, p);
    let u2 = mulmod(&b.x, &z1z1, p);
    let s2 = mulmod(&b.y, &mulmod(&a.z, &z1z1, p), p);
    let h = u2.submod(&a.x, p);
    let r = s2.submod(&a.y, p);
    if h.is_zero() {
        if r.is_zero() {
            return double(a, p);
        }
        return Pt::infinity();
    }
    let hh = sqmod(&h, p);
    let hhh = mulmod(&hh, &h, p);
    let v = mulmod(&a.x, &hh, p);
    // X3 = r^2 - H^3 - 2V
    let two_v = v.addmod(&v, p);
    let x3 = sqmod(&r, p).submod(&hhh, p).submod(&two_v, p);
    // Y3 = r(V - X3) - Y1*H^3
    let y3 = mulmod(&r, &v.submod(&x3, p), p).submod(&mulmod(&a.y, &hhh, p), p);
    // Z3 = Z1*H
    let z3 = mulmod(&a.z, &h, p);
    Pt { x: x3, y: y3, z: z3 }
}

/// General Jacobian + Jacobian addition (for combining scalar products).
fn add(a: &Pt, b: &Pt, p: &Big) -> Pt {
    if a.is_infinity() {
        return b.clone();
    }
    if b.is_infinity() {
        return a.clone();
    }
    let z1z1 = sqmod(&a.z, p);
    let z2z2 = sqmod(&b.z, p);
    let u1 = mulmod(&a.x, &z2z2, p);
    let u2 = mulmod(&b.x, &z1z1, p);
    let s1 = mulmod(&a.y, &mulmod(&b.z, &z2z2, p), p);
    let s2 = mulmod(&b.y, &mulmod(&a.z, &z1z1, p), p);
    let h = u2.submod(&u1, p);
    let r = s2.submod(&s1, p);
    if h.is_zero() {
        if r.is_zero() {
            return double(a, p);
        }
        return Pt::infinity();
    }
    let hh = sqmod(&h, p);
    let hhh = mulmod(&hh, &h, p);
    let v = mulmod(&u1, &hh, p);
    let two_v = v.addmod(&v, p);
    let x3 = sqmod(&r, p).submod(&hhh, p).submod(&two_v, p);
    let y3 = mulmod(&r, &v.submod(&x3, p), p).submod(&mulmod(&s1, &hhh, p), p);
    let z3 = mulmod(&a.z, &mulmod(&b.z, &h, p), p);
    Pt { x: x3, y: y3, z: z3 }
}

/// `k * pt` — MSB-first double-and-add.
fn scalar_mul(k: &Big, pt: &Pt, p: &Big) -> Pt {
    let mut acc = Pt::infinity();
    for i in (0..k.nbits()).rev() {
        acc = double(&acc, p);
        if k.bit(i) {
            acc = add_mixed(&acc, pt, p);
        }
    }
    acc
}

/// Project back to affine x. Returns `x mod n`-ready affine x coord.
fn affine_x(pt: &Pt, p: &Big) -> Result<Big> {
    if pt.is_infinity() {
        return Err(Error::Verify("ecdsa: point at infinity"));
    }
    let zi = pt.z.invmod(p);
    let zi2 = sqmod(&zi, p);
    Ok(mulmod(&pt.x, &zi2, p))
}

/// Verify an ECDSA signature `(r, s)` over `digest` with public key `q`.
///
/// `pubkey` is the ANSI X9.62 uncompressed point `04 || x || y`.
pub fn ecdsa_verify(
    curve: &Curve,
    pubkey: &[u8],
    digest: &[u8],
    r: &Big,
    s: &Big,
) -> Result<()> {
    let p = &curve.p;
    let n = &curve.n;
    if r.is_zero() || s.is_zero() || r.cmp_big(n) != std::cmp::Ordering::Less
        || s.cmp_big(n) != std::cmp::Ordering::Less
    {
        return Err(Error::Verify("ecdsa: r/s out of range"));
    }
    if pubkey.len() != 1 + 2 * curve.coord_len || pubkey[0] != 0x04 {
        return Err(Error::Unsupported("ecdsa: only uncompressed points"));
    }
    let qx = Big::from_be_bytes(&pubkey[1..1 + curve.coord_len]);
    let qy = Big::from_be_bytes(&pubkey[1 + curve.coord_len..]);
    // Sanity: Q must satisfy y^2 = x^3 - 3x + b.
    let b = curve_b(curve);
    let lhs = sqmod(&qy, p);
    let rhs = qx
        .mulmod(&qx, p)
        .mulmod(&qx, p)
        .submod(&Big::from_u64(3).mulmod(&qx, p), p)
        .addmod(&b, p);
    if lhs != rhs {
        return Err(Error::Verify("ecdsa: public key not on curve"));
    }

    // e = leftmost nbits(digest) bits of the hash.
    let e = truncate_digest(digest, n.nbits());
    let w = s.invmod(n);
    let u1 = e.mulmod(&w, n);
    let u2 = r.mulmod(&w, n);

    let g = Pt::affine(curve.gx.clone(), curve.gy.clone());
    let q = Pt::affine(qx, qy);
    let p1 = scalar_mul(&u1, &g, p);
    let p2 = scalar_mul(&u2, &q, p);
    let sum = add(&p1, &p2, p);
    let x = affine_x(&sum, p)?;
    let v = x.rem(n);
    if v == *r {
        Ok(())
    } else {
        Err(Error::Verify("ecdsa: signature mismatch"))
    }
}

/// Curve coefficient b (only used for the on-curve check).
fn curve_b(curve: &Curve) -> Big {
    match curve.coord_len {
        32 => big("5ac635d8aa3a93e7b3ebbd55769886bc651d06b0cc53b0f63bce3c3e27d2604b"),
        _ => big(
            "b3312fa7e23ee7e4988e056be3f82d19181d9c6efe8141120314088f5013875a\
             c656398d8a2ed19d2a85c8edd3ec2aef",
        ),
    }
}

/// `digest` interpreted big-endian, truncated to `nbits` bits.
fn truncate_digest(digest: &[u8], nbits: usize) -> Big {
    let mut d = digest.to_vec();
    let excess_bits = d.len() * 8 - nbits.min(d.len() * 8);
    if excess_bits > 0 {
        // shift right across the byte string
        let drop_bytes = excess_bits / 8;
        let shift = excess_bits % 8;
        d.truncate(d.len() - drop_bytes);
        if shift > 0 {
            let mut carry = 0u8;
            for b in d.iter_mut() {
                let next = *b & ((1 << shift) - 1);
                *b = (*b >> shift) | (carry << (8 - shift));
                carry = next;
            }
        }
    }
    Big::from_be_bytes(&d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sha2::sha256;

    /// Sign with a small "toy curve" to check the ECDSA math end-to-end:
    /// y^2 = x^3 - 3x + b over a small prime won't use our NIST curves,
    /// so instead verify P-256 against a signature produced by a known
    /// construction: check that verify rejects a corrupt signature, and
    /// that G*n = infinity (group order sanity via scalar_mul).
    #[test]
    fn p256_generator_order() {
        let c = p256();
        let g = Pt::affine(c.gx.clone(), c.gy.clone());
        // n*G should be the point at infinity.
        let inf = scalar_mul(&c.n, &g, &c.p);
        assert!(inf.is_infinity());
    }

    #[test]
    fn p256_generator_on_curve() {
        let c = p256();
        let b = curve_b(&c);
        let lhs = sqmod(&c.gy, &c.p);
        let rhs = c
            .gx
            .mulmod(&c.gx, &c.p)
            .mulmod(&c.gx, &c.p)
            .submod(&Big::from_u64(3).mulmod(&c.gx, &c.p), &c.p)
            .addmod(&b, &c.p);
        assert_eq!(lhs, rhs);
    }

    #[test]
    fn ecdsa_sign_and_verify_p256() {
        // Sign with our own scalar_mul: choose d=0x1dead.., compute Q=d*G,
        // sign with k=0xbeef.., verify. This only exercises consistency
        // (scalar_mul/verify agree) — real-world correctness additionally
        // relies on standard formulas.
        let c = p256();
        let d = big("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
        let k = big("fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210");
        let g = Pt::affine(c.gx.clone(), c.gy.clone());
        let q_pt = scalar_mul(&d, &g, &c.p);
        // affine-ize Q
        let zi = q_pt.z.invmod(&c.p);
        let zi2 = sqmod(&zi, &c.p);
        let zi3 = mulmod(&zi2, &zi, &c.p);
        let qx = mulmod(&q_pt.x, &zi2, &c.p);
        let qy = mulmod(&q_pt.y, &zi3, &c.p);
        let mut pubk = vec![0x04];
        pubk.extend_from_slice(&qx.to_be_bytes(32));
        pubk.extend_from_slice(&qy.to_be_bytes(32));

        let digest = sha256(b"hello ecdsa");
        let e = truncate_digest(&digest, c.n.nbits());
        // r = x-coord of k*G mod n
        let kg = scalar_mul(&k, &g, &c.p);
        let zi = kg.z.invmod(&c.p);
        let rx = mulmod(&kg.x, &sqmod(&zi, &c.p), &c.p).rem(&c.n);
        // s = k^-1 (e + d*r) mod n
        let s = k
            .invmod(&c.n)
            .mulmod(&e.addmod(&d.mulmod(&rx, &c.n), &c.n), &c.n);
        assert!(ecdsa_verify(&c, &pubk, &digest, &rx, &s).is_ok());
        // Corrupt s must fail.
        let bad_s = s.addmod(&Big::one(), &c.n);
        assert!(ecdsa_verify(&c, &pubk, &digest, &rx, &bad_s).is_err());
    }
}
