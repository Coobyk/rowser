//! AES-GCM authenticated encryption (NIST SP 800-38D).
//!
//! GHASH multiplication follows the spec's MSB-first bit ordering: blocks
//! are `u128`s whose bit 127 is the first bit of byte 0.

use crate::aes::Aes;

/// R = 0xe1 followed by zeros — the reduction constant for the GCM
/// polynomial x^128 + x^7 + x^2 + x + 1.
const R: u128 = 0xe1_00_00_00_00_00_00_00_00_00_00_00_00_00_00_00;

/// `x * y` in GF(2^128) with the GCM polynomial.
fn gf_mul(x: u128, y: u128) -> u128 {
    let mut z = 0u128;
    let mut v = y;
    for i in 0..128 {
        if (x >> (127 - i)) & 1 == 1 {
            z ^= v;
        }
        if v & 1 == 1 {
            v = (v >> 1) ^ R;
        } else {
            v >>= 1;
        }
    }
    z
}

fn u128_be(b: &[u8]) -> u128 {
    let mut a = [0u8; 16];
    a.copy_from_slice(&b[..16]);
    u128::from_be_bytes(a)
}

/// An AEAD instance: block cipher + precomputed GHASH subkey `H = E_K(0)`.
pub struct AesGcm {
    aes: Aes,
    h: u128,
}

impl AesGcm {
    pub fn new128(key: &[u8; 16]) -> AesGcm {
        Self::new(Aes::new128(key))
    }

    pub fn new256(key: &[u8; 32]) -> AesGcm {
        Self::new(Aes::new256(key))
    }

    fn new(aes: Aes) -> AesGcm {
        let mut zero = [0u8; 16];
        aes.encrypt_block(&mut zero);
        AesGcm {
            aes,
            h: u128_be(&zero),
        }
    }

    fn gctr(&self, mut icb: u128, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        let mut offset = 0;
        while offset < data.len() {
            let mut ks = icb.to_be_bytes();
            self.aes.encrypt_block(&mut ks);
            let n = (data.len() - offset).min(16);
            for i in 0..n {
                out.push(data[offset + i] ^ ks[i]);
            }
            // inc32: only the low 32 bits increment, modulo 2^32.
            icb = (icb & !0xffff_ffffu128) | (icb.wrapping_add(1) & 0xffff_ffff);
            offset += n;
        }
        out
    }

    fn ghash(&self, aad: &[u8], ct: &[u8]) -> u128 {
        let mut y = 0u128;
        let mut feed = |chunks: &[u8]| {
            let mut i = 0;
            while i < chunks.len() {
                let n = (chunks.len() - i).min(16);
                let mut block = [0u8; 16];
                block[..n].copy_from_slice(&chunks[i..i + n]);
                y = gf_mul(y ^ u128_be(&block), self.h);
                i += 16;
            }
        };
        feed(aad);
        feed(ct);
        let lens = ((aad.len() as u128) * 8) << 64 | (ct.len() as u128) * 8;
        y = gf_mul(y ^ lens, self.h);
        y
    }

    /// Returns `(ciphertext, tag)`.
    pub fn seal(&self, nonce: &[u8; 12], aad: &[u8], plaintext: &[u8]) -> (Vec<u8>, [u8; 16]) {
        let mut j0 = [0u8; 16];
        j0[..12].copy_from_slice(nonce);
        j0[15] = 1;
        let j0 = u128_be(&j0);
        let ct = self.gctr(j0.wrapping_add(1), plaintext);
        let s = self.ghash(aad, &ct);
        let mut ej0 = j0.to_be_bytes();
        self.aes.encrypt_block(&mut ej0);
        let tag_u = s ^ u128_be(&ej0);
        (ct, tag_u.to_be_bytes())
    }

    /// Verifies the tag and returns plaintext, or `None` on failure.
    pub fn open(
        &self,
        nonce: &[u8; 12],
        aad: &[u8],
        ciphertext: &[u8],
        tag: &[u8; 16],
    ) -> Option<Vec<u8>> {
        let mut j0 = [0u8; 16];
        j0[..12].copy_from_slice(nonce);
        j0[15] = 1;
        let j0 = u128_be(&j0);
        let s = self.ghash(aad, ciphertext);
        let mut ej0 = j0.to_be_bytes();
        self.aes.encrypt_block(&mut ej0);
        let expected = (s ^ u128_be(&ej0)).to_be_bytes();
        // Constant-time-ish compare.
        let mut diff = 0u8;
        for i in 0..16 {
            diff |= expected[i] ^ tag[i];
        }
        if diff != 0 {
            return None;
        }
        Some(self.gctr(j0.wrapping_add(1), ciphertext))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn hexify(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    // McGrew & Viega "The Galois/Counter Mode of Operation" test vectors.

    #[test]
    fn gcm_case1() {
        // K = 0, IV = 0, empty plaintext.
        let g = AesGcm::new128(&[0; 16]);
        let (ct, tag) = g.seal(&[0; 12], &[], &[]);
        assert!(ct.is_empty());
        assert_eq!(hexify(&tag), "58e2fccefa7e3061367f1d57a4e7455a");
    }

    #[test]
    fn gcm_case2() {
        let g = AesGcm::new128(&[0; 16]);
        let pt = [0u8; 16];
        let (ct, tag) = g.seal(&[0; 12], &[], &pt);
        assert_eq!(hexify(&ct), "0388dace60b6a392f328c2b971b2fe78");
        assert_eq!(hexify(&tag), "ab6e47d42cec13bdf53a67b21257bddf");
        assert_eq!(g.open(&[0; 12], &[], &ct, &tag).unwrap(), pt);
    }

    #[test]
    fn gcm_case3() {
        // Nonzero key, 64-byte plaintext, no AAD (spec Test Case 3).
        let key: [u8; 16] = hex("feffe9928665731c6d6a8f9467308308").try_into().unwrap();
        let iv: [u8; 12] = hex("cafebabefacedbaddecaf888").try_into().unwrap();
        let pt = hex(
            "d9313225f88406e5a55909c5aff5269a\
             86a7a9531534f7da2e4c303d8a318a72\
             1c3c0c95956809532fcf0e2449a6b525\
             b16aedf5aa0de657ba637b391aafd255",
        );
        let want_ct = hex(
            "42831ec2217774244b7221b784d0d49c\
             e3aa212f2c02a4e035c17e2329aca12e\
             21d514b25466931c7d8f6a5aac84aa05\
             1ba30b396a0aac973d58e091473f5985",
        );
        let want_tag = hex("4d5c2af327cd64a62cf35abd2ba6fab4");
        let g = AesGcm::new128(&key);
        let (ct, tag) = g.seal(&iv, &[], &pt);
        assert_eq!(ct, want_ct);
        assert_eq!(tag.to_vec(), want_tag);
        assert_eq!(g.open(&iv, &[], &ct, &tag).unwrap(), pt);
    }

    #[test]
    fn gcm_case4_aad() {
        // Same plaintext, with 20 bytes of AAD.
        let key: [u8; 16] = hex("feffe9928665731c6d6a8f9467308308").try_into().unwrap();
        let iv: [u8; 12] = hex("cafebabefacedbaddecaf888").try_into().unwrap();
        let pt = hex(
            "d9313225f88406e5a55909c5aff5269a\
             86a7a9531534f7da2e4c303d8a318a72\
             1c3c0c95956809532fcf0e2449a6b525\
             b16aedf5aa0de657ba637b39",
        );
        let aad = hex("feedfacedeadbeeffeedfacedeadbeefabaddad2");
        let want_ct = hex(
            "42831ec2217774244b7221b784d0d49c\
             e3aa212f2c02a4e035c17e2329aca12e\
             21d514b25466931c7d8f6a5aac84aa05\
             1ba30b396a0aac973d58e091",
        );
        let want_tag = hex("5bc94fbc3221a5db94fae95ae7121a47");
        let g = AesGcm::new128(&key);
        let (ct, tag) = g.seal(&iv, &aad, &pt);
        assert_eq!(ct, want_ct);
        assert_eq!(tag.to_vec(), want_tag);
        // Tag must fail with wrong AAD.
        assert!(g.open(&iv, b"wrong", &ct, &tag).is_none());
    }

    #[test]
    fn gcm_aes256() {
        // AES-256-GCM: 32-byte key, same plaintext + AAD.
        let key: [u8; 32] = hex(
            "feffe9928665731c6d6a8f9467308308\
             feffe9928665731c6d6a8f9467308308",
        )
        .try_into()
        .unwrap();
        let iv: [u8; 12] = hex("cafebabefacedbaddecaf888").try_into().unwrap();
        let pt = hex(
            "d9313225f88406e5a55909c5aff5269a\
             86a7a9531534f7da2e4c303d8a318a72\
             1c3c0c95956809532fcf0e2449a6b525\
             b16aedf5aa0de657ba637b39",
        );
        let aad = hex("feedfacedeadbeeffeedfacedeadbeefabaddad2");
        let want_ct = hex(
            "522dc1f099567d07f47f37a32a84427d\
             643a8cdcbfe5c0c97598a2bd2555d1aa\
             8cb08e48590dbb3da7b08b1056828838\
             c5f61e6393ba7a0abcc9f662",
        );
        let want_tag = hex("76fc6ece0f4e1768cddf8853bb2d551b");
        let g = AesGcm::new256(&key);
        let (ct, tag) = g.seal(&iv, &aad, &pt);
        assert_eq!(ct, want_ct);
        assert_eq!(tag.to_vec(), want_tag);
        assert_eq!(g.open(&iv, &aad, &ct, &tag).unwrap(), pt);
    }
}
