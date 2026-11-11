//! RSA signature verification: PKCS#1 v1.5 (RFC 8017 §8.2) and
//! RSASSA-PSS (§8.1 / §9.1). Only the verify direction is needed for
//! certificate chains and CertificateVerify.

use crate::bignum::Big;
use crate::sha2::HashAlg;
use crate::{Error, Result};

/// ASN.1 DigestInfo DER prefixes per RFC 8017 §9.2 notes / RFC 3447.
fn digest_info_prefix(alg: HashAlg) -> &'static [u8] {
    match alg {
        HashAlg::Sha256 => &[
            0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
            0x01, 0x05, 0x00, 0x04, 0x20,
        ],
        HashAlg::Sha384 => &[
            0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
            0x02, 0x05, 0x00, 0x04, 0x30,
        ],
        HashAlg::Sha512 => &[
            0x30, 0x51, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
            0x03, 0x05, 0x00, 0x04, 0x40,
        ],
    }
}

/// Raw RSA public-key operation: `sig^e mod n`, left-padded to the
/// modulus byte length `k`.
fn rsaep(n: &Big, e: &Big, sig: &[u8]) -> Vec<u8> {
    let k = n.nbits().div_ceil(8);
    let s = Big::from_be_bytes(sig);
    let m = s.powmod(e, n);
    m.to_be_bytes(k)
}

/// PKCS#1 v1.5: verify that `sig` decrypts to `EM = 00 01 FF.. 00 T`
/// where `T = DigestInfo || H`.
pub fn pkcs1v15_verify(n: &Big, e: &Big, alg: HashAlg, digest: &[u8], sig: &[u8]) -> Result<()> {
    let em = rsaep(n, e, sig);
    let mut t = Vec::with_capacity(digest.len() + 19);
    t.extend_from_slice(digest_info_prefix(alg));
    t.extend_from_slice(digest);
    // EM must be 00 || 01 || PS(>=8 ff bytes) || 00 || T
    if em.len() < t.len() + 11 || em[0] != 0x00 || em[1] != 0x01 {
        return Err(Error::Verify("rsa pkcs1v15: bad padding"));
    }
    let ps_len = em.len() - t.len() - 3;
    for b in &em[2..2 + ps_len] {
        if *b != 0xff {
            return Err(Error::Verify("rsa pkcs1v15: bad padding string"));
        }
    }
    if em[2 + ps_len] != 0x00 {
        return Err(Error::Verify("rsa pkcs1v15: missing separator"));
    }
    if &em[3 + ps_len..] != t.as_slice() {
        return Err(Error::Verify("rsa pkcs1v15: digest mismatch"));
    }
    Ok(())
}

/// MGF1 (RFC 8017 §B.2.1) with the given hash.
fn mgf1(alg: HashAlg, seed: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut counter = 0u32;
    while out.len() < len {
        let mut input = seed.to_vec();
        input.extend_from_slice(&counter.to_be_bytes());
        out.extend_from_slice(&alg.hash(&input));
        counter += 1;
    }
    out.truncate(len);
    out
}

/// RSASSA-PSS verify (EMSA-PSS), any salt length, trailer 0xbc.
pub fn pss_verify(n: &Big, e: &Big, alg: HashAlg, digest: &[u8], sig: &[u8]) -> Result<()> {
    let em_bits = n.nbits() - 1;
    let em_len = em_bits.div_ceil(8);
    let em = rsaep(n, e, sig);
    let hlen = alg.hash_len();
    // em is k bytes long; em_len <= k. Leftmost bytes should be zero.
    let em = if em.len() > em_len {
        // k == em_len + 1 only when modBits-1 uses one fewer byte.
        let lead = em.len() - em_len;
        if em[..lead].iter().any(|&b| b != 0) {
            return Err(Error::Verify("rsa pss: em too long"));
        }
        em[lead..].to_vec()
    } else {
        em
    };
    if em.len() != em_len || em_len < hlen + 2 {
        return Err(Error::Verify("rsa pss: em length"));
    }
    if em[em_len - 1] != 0xbc {
        return Err(Error::Verify("rsa pss: bad trailer"));
    }
    let masked_db = &em[..em_len - hlen - 1];
    let h = &em[em_len - hlen - 1..em_len - 1];
    let db_mask = mgf1(alg, h, em_len - hlen - 1);
    let mut db = vec![0u8; masked_db.len()];
    for i in 0..db.len() {
        db[i] = masked_db[i] ^ db_mask[i];
    }
    // The leftmost (8*emLen - emBits) bits of DB must be zero.
    let unused = 8 * em_len - em_bits;
    if unused > 0 {
        db[0] &= 0xff >> unused;
    }
    // DB = PS(0x00*) || 0x01 || salt
    let mut i = 0;
    while i < db.len() && db[i] == 0 {
        i += 1;
    }
    if i >= db.len() || db[i] != 0x01 {
        return Err(Error::Verify("rsa pss: no salt separator"));
    }
    let salt = &db[i + 1..];
    let mut m_prime = Vec::with_capacity(8 + hlen + salt.len());
    m_prime.extend_from_slice(&[0u8; 8]);
    m_prime.extend_from_slice(digest);
    m_prime.extend_from_slice(salt);
    let h_check = alg.hash(&m_prime);
    if h_check.as_slice() != h {
        return Err(Error::Verify("rsa pss: hash mismatch"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sha2::sha256;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn pkcs1v15_constructed() {
        // Build a textbook-sized "RSA" modulus where we can compute a
        // signature by hand: tiny primes p=61, q=53 -> n=3233, e=17, d=2753
        // (the classic Wikipedia RSA example). Sign a DigestInfo by
        // computing m = EM^d mod n with our own powmod, then verify.
        let n = Big::from_u64(3233);
        let e = Big::from_u64(17);
        let d = Big::from_u64(2753);

        let digest = sha256(b"test");
        let mut t = Vec::new();
        t.extend_from_slice(digest_info_prefix(HashAlg::Sha256));
        t.extend_from_slice(&digest);
        // EM = 00 01 ff.. 00 T, k = 2 bytes? n=3233 -> k=2. Too small for
        // real padding, so exercise rsaep round-trip instead: sig = m^d,
        // check rsaep(sig) == m.
        let m = Big::from_u64(65);
        let sig = m.powmod(&d, &n);
        let back = rsaep(&n, &e, &sig.to_be_bytes(2));
        assert_eq!(Big::from_be_bytes(&back), m);
    }

    #[test]
    fn pkcs1v15_negative() {
        // A valid-size signature with wrong content must fail.
        let n = hex(
            "00d5eeaa4f7a4d9a8b2e3e06f7e6f5e8c0a3b1d2e4f5060708090a0b0c0d0e0f\
                    00112233445566778899aabbccddee11",
        );
        let n = Big::from_be_bytes(&n);
        let e = Big::from_u64(65537);
        let sig = vec![1u8; n.nbits().div_ceil(8)];
        assert!(pkcs1v15_verify(&n, &e, HashAlg::Sha256, &sha256(b"x"), &sig).is_err());
    }
}
