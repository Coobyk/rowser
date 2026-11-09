//! HKDF (RFC 5869) and the TLS 1.3 key-schedule helpers (RFC 8446 §7.1).

use crate::hmac::hmac;
use crate::sha2::HashAlg;
use crate::{Error, Result};

/// `HKDF-Extract(salt, ikm)`. An empty salt means `HashLen` zeros.
pub fn extract(alg: HashAlg, salt: &[u8], ikm: &[u8]) -> Vec<u8> {
    let zeros;
    let salt = if salt.is_empty() {
        zeros = vec![0u8; alg.hash_len()];
        &zeros
    } else {
        salt
    };
    hmac(alg, salt, ikm)
}

/// `HKDF-Expand(prk, info, len)`.
pub fn expand(alg: HashAlg, prk: &[u8], info: &[u8], len: usize) -> Result<Vec<u8>> {
    let hash_len = alg.hash_len();
    if len > 255 * hash_len {
        return Err(Error::Unsupported("hkdf expand length > 255*hashlen"));
    }
    let mut out = Vec::with_capacity(len);
    let mut t: Vec<u8> = Vec::new();
    let mut i = 1u8;
    while out.len() < len {
        let mut data = t.clone();
        data.extend_from_slice(info);
        data.push(i);
        t = hmac(alg, prk, &data);
        out.extend_from_slice(&t);
        i = i.wrapping_add(1);
    }
    out.truncate(len);
    Ok(out)
}

/// `HKDF-Expand-Label(secret, label, context, len)` — RFC 8446 §7.1.
pub fn expand_label(
    alg: HashAlg,
    secret: &[u8],
    label: &str,
    context: &[u8],
    len: usize,
) -> Result<Vec<u8>> {
    let mut full_label = Vec::with_capacity(6 + label.len());
    full_label.extend_from_slice(b"tls13 ");
    full_label.extend_from_slice(label.as_bytes());
    if full_label.len() > 255 || context.len() > 255 || len > u16::MAX as usize {
        return Err(Error::Unsupported("hkdf expand_label too long"));
    }
    let mut info = Vec::new();
    info.extend_from_slice(&(len as u16).to_be_bytes());
    info.push(full_label.len() as u8);
    info.extend_from_slice(&full_label);
    info.push(context.len() as u8);
    info.extend_from_slice(context);
    expand(alg, secret, &info, len)
}

/// `Derive-Secret(secret, label, messages_hash)` where `messages_hash` is the
/// transcript hash the caller already computed (empty hash for "derived").
pub fn derive_secret(
    alg: HashAlg,
    secret: &[u8],
    label: &str,
    messages_hash: &[u8],
) -> Result<Vec<u8>> {
    expand_label(alg, secret, label, messages_hash, alg.hash_len())
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

    #[test]
    fn rfc5869_case1() {
        // SHA-256, IKM = 0x0b*22, salt = 0x00..0x0c, info = 0xf0..0xf9, L = 42
        let ikm = [0x0bu8; 22];
        let salt = hex("000102030405060708090a0b0c");
        let info = hex("f0f1f2f3f4f5f6f7f8f9");
        let prk = extract(HashAlg::Sha256, &salt, &ikm);
        assert_eq!(
            hexify(&prk),
            "077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5"
        );
        let okm = expand(HashAlg::Sha256, &prk, &info, 42).unwrap();
        assert_eq!(
            hexify(&okm),
            "3cb25f25faacd57a90434f64d0362f2a\
             2d2d0a90cf1a5a4c5db02d56ecc4c5bf\
             34007208d5b887185865"
        );
    }

    #[test]
    fn rfc5869_case3_no_salt_no_info() {
        // IKM = 0x0b*22, salt = empty, info = empty, L = 42
        let ikm = [0x0bu8; 22];
        let prk = extract(HashAlg::Sha256, &[], &ikm);
        assert_eq!(
            hexify(&prk),
            "19ef24a32c717b167f33a91d6f648bdf96596776afdb6377ac434c1c293ccb04"
        );
        let okm = expand(HashAlg::Sha256, &prk, &[], 42).unwrap();
        assert_eq!(
            hexify(&okm),
            "8da4e775a563c18f715f802a063c5a31\
             b8a11f5c5ee1879ec3454e5f3c738d2d\
             9d201395faa4b61a96c8"
        );
    }
}
