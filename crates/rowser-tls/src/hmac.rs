//! HMAC (RFC 2104) over the SHA-2 family.

use crate::sha2::HashAlg;

/// `HMAC_hash(key, msg)` — allocates; fine for the small inputs TLS uses.
pub fn hmac(alg: HashAlg, key: &[u8], msg: &[u8]) -> Vec<u8> {
    let block = alg.block_size();
    let mut k = if key.len() > block {
        alg.hash(key)
    } else {
        key.to_vec()
    };
    k.resize(block, 0);
    let mut inner = Vec::with_capacity(block + msg.len());
    inner.extend(k.iter().map(|b| b ^ 0x36));
    inner.extend_from_slice(msg);
    let inner_hash = alg.hash(&inner);
    let mut outer = Vec::with_capacity(block + inner_hash.len());
    outer.extend(k.iter().map(|b| b ^ 0x5c));
    outer.extend_from_slice(&inner_hash);
    alg.hash(&outer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hexify(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn rfc4231_case1() {
        // key = 0x0b * 20, data = "Hi There"
        let out = hmac(HashAlg::Sha256, &[0x0b; 20], b"Hi There");
        assert_eq!(
            hexify(&out),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn rfc4231_case2() {
        let out = hmac(HashAlg::Sha256, b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hexify(&out),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn rfc4231_case6_long_key() {
        // key = 0xaa * 131 (> block size) exercises the key-hashing path.
        let key = [0xaau8; 131];
        let out = hmac(
            HashAlg::Sha256,
            &key,
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        assert_eq!(
            hexify(&out),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn hmac_sha384() {
        // RFC 4231 case 1 with SHA-384.
        let out = hmac(HashAlg::Sha384, &[0x0b; 20], b"Hi There");
        assert_eq!(
            hexify(&out),
            "afd03944d84895626b0825f4ab46907f15f9dadbe4101ec682aa034c7cebc59c\
             faea9ea9076ede7f4af152e8b2fa9cb6"
        );
    }
}
