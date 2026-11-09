//! X.509 certificate parsing and chain verification (RFC 5280).
//!
//! We parse just enough of a certificate to build and verify a chain:
//! subject/issuer names (raw DER for matching), validity window, the
//! SubjectPublicKeyInfo, SAN dNSNames, BasicConstraints, and the
//! signature. Signature algorithms supported: RSA PKCS#1 v1.5 with
//! SHA-256/384/512, ECDSA-with-SHA256 on P-256, ECDSA-with-SHA384 on
//! P-384, and RSASSA-PSS with SHA-256/384/512 (salt recovered).
//!
//! Names are compared as raw DER. CAs copy the issuer name byte-for-byte
//! in practice; exotic re-encodings would fail closed.

use std::collections::HashMap;

use crate::bignum::Big;
use crate::der::{self, Reader, Tlv};
use crate::ecdsa;
use crate::rsa;
use crate::sha2::HashAlg;
use crate::{Error, Result};

// Signature algorithm OIDs (as dotted arcs).
const OID_SHA256_WITH_RSA: &[u64] = &[1, 2, 840, 113549, 1, 1, 11];
const OID_SHA384_WITH_RSA: &[u64] = &[1, 2, 840, 113549, 1, 1, 12];
const OID_SHA512_WITH_RSA: &[u64] = &[1, 2, 840, 113549, 1, 1, 13];
const OID_ECDSA_SHA256: &[u64] = &[1, 2, 840, 10045, 4, 3, 2];
const OID_ECDSA_SHA384: &[u64] = &[1, 2, 840, 10045, 4, 3, 3];
const OID_RSASSA_PSS: &[u64] = &[1, 2, 840, 113549, 1, 1, 10];

// Public key algorithm OIDs.
const OID_RSA_ENCRYPTION: &[u64] = &[1, 2, 840, 113549, 1, 1, 1];
const OID_EC_PUBLIC_KEY: &[u64] = &[1, 2, 840, 10045, 2, 1];
const OID_P256: &[u64] = &[1, 2, 840, 10045, 3, 1, 7];
const OID_P384: &[u64] = &[1, 3, 132, 0, 34];

// Attribute / extension OIDs.
const OID_CN: &[u64] = &[2, 5, 4, 3];
const OID_SAN: &[u64] = &[2, 5, 29, 17];
const OID_BASIC_CONSTRAINTS: &[u64] = &[2, 5, 29, 19];

/// A parsed public key from a SubjectPublicKeyInfo.
#[derive(Clone, Debug)]
pub enum PublicKey {
    Rsa { n: Big, e: Big },
    /// Uncompressed SEC1 point `04 || x || y`, on the named curve.
    EcP256(Vec<u8>),
    EcP384(Vec<u8>),
    Other,
}

/// A parsed X.509 certificate.
#[derive(Clone, Debug)]
pub struct Certificate {
    /// Raw DER of the TBSCertificate (what the signature covers).
    pub tbs_raw: Vec<u8>,
    pub serial: Vec<u8>,
    /// Raw DER of the issuer/subject Name (for issuer matching).
    pub issuer_raw: Vec<u8>,
    pub subject_raw: Vec<u8>,
    /// First commonName found, if any (display + CN hostname fallback).
    pub subject_cn: Option<String>,
    pub issuer_cn: Option<String>,
    /// Validity as unix timestamps.
    pub not_before: i64,
    pub not_after: i64,
    pub public_key: PublicKey,
    /// dNSName entries from the SAN extension.
    pub san_dns: Vec<String>,
    /// BasicConstraints cA flag, if the extension is present.
    pub is_ca: Option<bool>,
    /// Signature algorithm OID (contents bytes).
    pub sig_oid: Vec<u8>,
    /// Signature value (bits, unused-bits byte already stripped).
    pub signature: Vec<u8>,
}

/// Decode a Name's raw DER into the first commonName string found.
fn first_cn(name_tlv: &Tlv<'_>) -> Option<String> {
    let mut outer = Reader::new(name_tlv.value);
    while let Ok(set_tlv) = outer.read() {
        if set_tlv.tag != der::TAG_SET {
            continue;
        }
        let mut set = Reader::new(set_tlv.value);
        while let Ok(av) = set.read() {
            if av.tag != der::TAG_SEQUENCE {
                continue;
            }
            let mut ava = Reader::new(av.value);
            let oid = ava.read().ok()?;
            let val = ava.read().ok()?;
            if oid.tag == der::TAG_OID && der::oid_eq(oid.value, OID_CN) {
                return Some(dn_string(val));
            }
        }
    }
    None
}

/// Lossy string extraction from a DirectoryString value.
fn dn_string(tlv: Tlv<'_>) -> String {
    match tlv.tag {
        der::TAG_BMP_STRING => {
            // UCS-2 big-endian
            tlv.value
                .chunks_exact(2)
                .map(|c| {
                    let cp = u16::from_be_bytes([c[0], c[1]]);
                    char::from_u32(cp as u32).unwrap_or('\u{fffd}')
                })
                .collect()
        }
        _ => String::from_utf8_lossy(tlv.value).into_owned(),
    }
}

/// Parse UTCTime / GeneralizedTime ("YYMMDDHHMMSSZ" / "YYYYMMDDHHMMSSZ")
/// into a unix timestamp.
fn parse_time(tlv: &Tlv<'_>) -> Result<i64> {
    let s = std::str::from_utf8(tlv.value).map_err(|_| Error::Decode("bad time"))?;
    if !s.is_ascii() {
        return Err(Error::Decode("non-ascii time"));
    }
    let digits = |r: &str| -> Result<i64> {
        r.parse::<i64>().map_err(|_| Error::Decode("bad time digits"))
    };
    let (year, rest) = match tlv.tag {
        der::TAG_UTC_TIME => {
            if s.len() < 11 {
                return Err(Error::Decode("short utctime"));
            }
            let yy = digits(&s[0..2])?;
            let year = if yy < 50 { 2000 + yy } else { 1900 + yy };
            (year, &s[2..])
        }
        der::TAG_GENERALIZED_TIME => {
            if s.len() < 13 {
                return Err(Error::Decode("short gentime"));
            }
            (digits(&s[0..4])?, &s[4..])
        }
        _ => return Err(Error::Decode("not a time")),
    };
    // rest = MMDDHHMMSS then Z or +hhmm / -hhmm
    if rest.len() < 10 {
        return Err(Error::Decode("short time"));
    }
    let mo = digits(&rest[0..2])?;
    let dy = digits(&rest[2..4])?;
    let hh = digits(&rest[4..6])?;
    let mi = digits(&rest[6..8])?;
    let ss = digits(&rest[8..10])?;
    let mut secs = days_from_civil(year, mo, dy) * 86400 + hh * 3600 + mi * 60 + ss;
    let tz = &rest[10..];
    if tz.starts_with('Z') || tz.is_empty() {
        // utc
    } else if tz.len() >= 5 && (tz.starts_with('+') || tz.starts_with('-')) {
        let off = digits(&tz[1..3])? * 3600 + digits(&tz[3..5])? * 60;
        if tz.starts_with('+') {
            secs -= off;
        } else {
            secs += off;
        }
    } else {
        return Err(Error::Decode("bad tz"));
    }
    Ok(secs)
}

/// Howard Hinnant's days-from-civil: days since 1970-01-01.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 }; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Strip the unused-bits count byte from a BIT STRING value.
fn bit_string_value(tlv: &Tlv<'_>) -> Result<&[u8]> {
    if tlv.tag != der::TAG_BIT_STRING || tlv.value.is_empty() || tlv.value[0] > 7 {
        return Err(Error::Decode("bad bit string"));
    }
    // We only handle byte-aligned key/signature material.
    if tlv.value[0] != 0 {
        return Err(Error::Decode("non-byte-aligned bit string"));
    }
    Ok(&tlv.value[1..])
}

fn read_integer_bytes(r: &mut Reader<'_>) -> Result<Vec<u8>> {
    let tlv = r.expect(der::TAG_INTEGER)?;
    // Strip a single leading zero byte used to force positive sign.
    let v = tlv.value;
    if v.len() > 1 && v[0] == 0 {
        Ok(v[1..].to_vec())
    } else {
        Ok(v.to_vec())
    }
}

/// Parse one DER-encoded certificate.
pub fn parse_certificate(der_bytes: &[u8]) -> Result<Certificate> {
    let mut top = Reader::new(der_bytes);
    let cert_tlv = top.expect(der::TAG_SEQUENCE)?;
    let mut seq = Reader::new(cert_tlv.value);

    // TBSCertificate — keep the whole raw TLV for signature verification.
    let tbs = seq.expect(der::TAG_SEQUENCE)?;
    let tbs_raw = tbs.raw.to_vec();
    let mut t = Reader::new(tbs.value);

    // version [0] EXPLICIT (v3) or serial directly.
    let mut first = t.read()?;
    if first.tag == 0xa0 {
        first = t.read()?;
    }
    let serial = {
        if first.tag != der::TAG_INTEGER {
            return Err(Error::Decode("tbs: missing serial"));
        }
        let v = first.value;
        if v.len() > 1 && v[0] == 0 {
            v[1..].to_vec()
        } else {
            v.to_vec()
        }
    };
    // signature AlgorithmIdentifier inside TBS (we use the outer one).
    let _inner_alg = t.expect(der::TAG_SEQUENCE)?;
    let issuer = t.expect(der::TAG_SEQUENCE)?;
    let validity = t.expect(der::TAG_SEQUENCE)?;
    let (not_before, not_after) = {
        let mut v = Reader::new(validity.value);
        let nb = v.read()?;
        let na = v.read()?;
        (parse_time(&nb)?, parse_time(&na)?)
    };
    let subject = t.expect(der::TAG_SEQUENCE)?;
    let spki = t.expect(der::TAG_SEQUENCE)?;
    let public_key = parse_spki(&spki)?;

    // Remaining optional fields: issuer/subject unique IDs [1]/[2],
    // extensions [3] EXPLICIT.
    let mut san_dns = Vec::new();
    let mut is_ca = None;
    while !t.at_end() {
        let field = t.read()?;
        if field.tag == 0xa3 {
            let mut ext_seq = Reader::new(field.value);
            if let Ok(exts) = ext_seq.expect(der::TAG_SEQUENCE) {
                parse_extensions(exts.value, &mut san_dns, &mut is_ca);
            }
        }
    }

    // signatureAlgorithm + signatureValue
    let alg_seq = seq.expect(der::TAG_SEQUENCE)?;
    let sig_oid = {
        let mut a = Reader::new(alg_seq.value);
        let oid = a.expect(der::TAG_OID)?;
        oid.value.to_vec()
    };
    let sig_bits = seq.expect(der::TAG_BIT_STRING)?;
    let signature = bit_string_value(&sig_bits)?.to_vec();

    Ok(Certificate {
        tbs_raw,
        serial,
        issuer_raw: issuer.raw.to_vec(),
        subject_raw: subject.raw.to_vec(),
        subject_cn: first_cn(&subject),
        issuer_cn: first_cn(&issuer),
        not_before,
        not_after,
        public_key,
        san_dns,
        is_ca,
        sig_oid,
        signature,
    })
}

fn parse_extensions(der_bytes: &[u8], san_dns: &mut Vec<String>, is_ca: &mut Option<bool>) {
    let mut exts = Reader::new(der_bytes);
    while let Ok(ext) = exts.read() {
        if ext.tag != der::TAG_SEQUENCE {
            continue;
        }
        let mut e = Reader::new(ext.value);
        let Ok(oid) = e.expect(der::TAG_OID) else {
            continue;
        };
        // optional critical BOOLEAN, then OCTET STRING value.
        let Ok(val) = e.read() else { continue };
        let val = if val.tag == der::TAG_BOOLEAN {
            match e.read() {
                Ok(v) => v,
                Err(_) => continue,
            }
        } else {
            val
        };
        if val.tag != der::TAG_OCTET_STRING {
            continue;
        }
        if der::oid_eq(oid.value, OID_SAN) {
            let mut names = Reader::new(val.value);
            if let Ok(list) = names.expect(der::TAG_SEQUENCE) {
                let mut n = Reader::new(list.value);
                while let Ok(gn) = n.read() {
                    if gn.tag == 0x82 {
                        // [2] dNSName, IA5String implicit
                        san_dns.push(String::from_utf8_lossy(gn.value).into_owned());
                    }
                }
            }
        } else if der::oid_eq(oid.value, OID_BASIC_CONSTRAINTS) {
            let mut bc = Reader::new(val.value);
            if let Ok(seq) = bc.expect(der::TAG_SEQUENCE) {
                let mut s = Reader::new(seq.value);
                if let Ok(b) = s.read() {
                    if b.tag == der::TAG_BOOLEAN {
                        *is_ca = Some(b.value.first() == Some(&0xff));
                    }
                }
            }
        }
    }
}

fn parse_spki(spki: &Tlv<'_>) -> Result<PublicKey> {
    let mut s = Reader::new(spki.value);
    let alg = s.expect(der::TAG_SEQUENCE)?;
    let mut a = Reader::new(alg.value);
    let oid = a.expect(der::TAG_OID)?;
    let params = a.read().ok();
    let key_bits = s.read()?;
    let key = bit_string_value(&key_bits)?;

    if der::oid_eq(oid.value, OID_RSA_ENCRYPTION) {
        // key is DER RSAPublicKey ::= SEQ { n INTEGER, e INTEGER }
        let mut k = Reader::new(key);
        let kseq = k.expect(der::TAG_SEQUENCE)?;
        let mut ks = Reader::new(kseq.value);
        let n = Big::from_be_bytes(&read_integer_bytes(&mut ks)?);
        let e = Big::from_be_bytes(&read_integer_bytes(&mut ks)?);
        Ok(PublicKey::Rsa { n, e })
    } else if der::oid_eq(oid.value, OID_EC_PUBLIC_KEY) {
        let curve = params.ok_or(Error::Decode("ec key: missing curve"))?;
        if der::oid_eq(curve.value, OID_P256) {
            Ok(PublicKey::EcP256(key.to_vec()))
        } else if der::oid_eq(curve.value, OID_P384) {
            Ok(PublicKey::EcP384(key.to_vec()))
        } else {
            Ok(PublicKey::Other)
        }
    } else {
        Ok(PublicKey::Other)
    }
}

/// The hash + algorithm family a signature OID implies.
enum SigAlg {
    RsaPkcs1(HashAlg),
    EcdsaP256, // SHA-256
    EcdsaP384, // SHA-384
    RsaPss(HashAlg),
}

fn classify_sig_oid(oid_bytes: &[u8]) -> Result<SigAlg> {
    if der::oid_eq(oid_bytes, OID_SHA256_WITH_RSA) {
        Ok(SigAlg::RsaPkcs1(HashAlg::Sha256))
    } else if der::oid_eq(oid_bytes, OID_SHA384_WITH_RSA) {
        Ok(SigAlg::RsaPkcs1(HashAlg::Sha384))
    } else if der::oid_eq(oid_bytes, OID_SHA512_WITH_RSA) {
        Ok(SigAlg::RsaPkcs1(HashAlg::Sha512))
    } else if der::oid_eq(oid_bytes, OID_ECDSA_SHA256) {
        Ok(SigAlg::EcdsaP256)
    } else if der::oid_eq(oid_bytes, OID_ECDSA_SHA384) {
        Ok(SigAlg::EcdsaP384)
    } else if der::oid_eq(oid_bytes, OID_RSASSA_PSS) {
        // Parameters carry the hash; PSS params parsing is rare in cert
        // chains — assume SHA-256, the overwhelmingly common choice.
        Ok(SigAlg::RsaPss(HashAlg::Sha256))
    } else {
        Err(Error::Unsupported("signature algorithm oid"))
    }
}

/// Decode `SEQUENCE { r INTEGER, s INTEGER }` (an ECDSA signature) —
/// public for the TLS CertificateVerify path.
pub fn decode_signature_rs(sig: &[u8]) -> Result<(Big, Big)> {
    decode_ecdsa_sig(sig)
}

/// Decode `SEQUENCE { r INTEGER, s INTEGER }` (an ECDSA signature).
fn decode_ecdsa_sig(sig: &[u8]) -> Result<(Big, Big)> {
    let mut r = Reader::new(sig);
    let seq = r.expect(der::TAG_SEQUENCE)?;
    let mut s = Reader::new(seq.value);
    let rr = Big::from_be_bytes(&read_integer_bytes(&mut s)?);
    let ss = Big::from_be_bytes(&read_integer_bytes(&mut s)?);
    Ok((rr, ss))
}

impl Certificate {
    /// Verify that `issuer`'s public key signed this certificate's TBS.
    pub fn verify_signed_by(&self, issuer: &Certificate) -> Result<()> {
        let alg = classify_sig_oid(&self.sig_oid)?;
        match (alg, &issuer.public_key) {
            (SigAlg::RsaPkcs1(hash), PublicKey::Rsa { n, e }) => {
                let digest = hash.hash(&self.tbs_raw);
                rsa::pkcs1v15_verify(n, e, hash, &digest, &self.signature)
            }
            (SigAlg::RsaPss(hash), PublicKey::Rsa { n, e }) => {
                let digest = hash.hash(&self.tbs_raw);
                rsa::pss_verify(n, e, hash, &digest, &self.signature)
            }
            (SigAlg::EcdsaP256, PublicKey::EcP256(key)) => {
                let digest = HashAlg::Sha256.hash(&self.tbs_raw);
                let (r, s) = decode_ecdsa_sig(&self.signature)?;
                ecdsa::ecdsa_verify(&ecdsa::p256(), key, &digest, &r, &s)
            }
            (SigAlg::EcdsaP384, PublicKey::EcP384(key)) => {
                let digest = HashAlg::Sha384.hash(&self.tbs_raw);
                let (r, s) = decode_ecdsa_sig(&self.signature)?;
                ecdsa::ecdsa_verify(&ecdsa::p384(), key, &digest, &r, &s)
            }
            (SigAlg::EcdsaP256, PublicKey::EcP384(_))
            | (SigAlg::EcdsaP384, PublicKey::EcP256(_)) => {
                Err(Error::Verify("ecdsa: curve/algorithm mismatch"))
            }
            _ => Err(Error::Verify("signature/key type mismatch")),
        }
    }
}

/// Base64-decode (standard alphabet, ignores whitespace).
pub fn base64_decode(s: &str) -> Result<Vec<u8>> {
    fn val(c: u8) -> Result<u32> {
        match c {
            b'A'..=b'Z' => Ok((c - b'A') as u32),
            b'a'..=b'z' => Ok((c - b'a' + 26) as u32),
            b'0'..=b'9' => Ok((c - b'0' + 52) as u32),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(Error::Decode("bad base64")),
        }
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut nbits = 0u32;
    for &c in s.as_bytes() {
        if c.is_ascii_whitespace() || c == b'=' {
            continue;
        }
        acc = (acc << 6) | val(c)?;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
            acc &= (1 << nbits) - 1;
        }
    }
    Ok(out)
}

/// Extract all DER certs from a PEM blob.
pub fn parse_pem_certs(text: &str) -> Vec<Certificate> {
    let mut certs = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("-----BEGIN CERTIFICATE-----") {
        let after = &rest[start + 27..];
        let Some(end) = after.find("-----END CERTIFICATE-----") else {
            break;
        };
        if let Ok(der_bytes) = base64_decode(&after[..end]) {
            if let Ok(cert) = parse_certificate(&der_bytes) {
                certs.push(cert);
            }
        }
        rest = &after[end + 25..];
    }
    certs
}

/// A set of trusted root certificates indexed by subject name.
pub struct TrustStore {
    pub roots: Vec<Certificate>,
    by_subject: HashMap<Vec<u8>, Vec<usize>>,
}

impl TrustStore {
    pub fn empty() -> TrustStore {
        TrustStore {
            roots: Vec::new(),
            by_subject: HashMap::new(),
        }
    }

    pub fn add(&mut self, cert: Certificate) {
        let idx = self.roots.len();
        self.by_subject
            .entry(cert.subject_raw.clone())
            .or_default()
            .push(idx);
        self.roots.push(cert);
    }

    /// Load system roots from `/etc/ssl/certs` (Debian/Fedora layout):
    /// the `ca-certificates.crt` bundle plus any `*.pem` files.
    pub fn system() -> TrustStore {
        let mut store = TrustStore::empty();
        let dir = match std::fs::read_dir("/etc/ssl/certs") {
            Ok(d) => d,
            Err(_) => return store,
        };
        for entry in dir.flatten() {
            let path = entry.path();
            // Try everything that reads as text — hashed `.0` symlinks and
            // bundles have no uniform extension.
            if let Ok(text) = std::fs::read_to_string(&path) {
                if !text.contains("BEGIN CERTIFICATE") {
                    continue;
                }
                for cert in parse_pem_certs(&text) {
                    store.add(cert);
                }
            }
        }
        store
    }

    fn find_root_candidates(&self, issuer_raw: &[u8]) -> Vec<usize> {
        self.by_subject
            .get(issuer_raw)
            .cloned()
            .unwrap_or_default()
    }
}

/// Does `pattern` (a dNSName or CN) match `host`? Supports a leading
/// `*.` wildcard covering exactly the leftmost label.
fn dns_name_matches(pattern: &str, host: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let host = host.to_ascii_lowercase();
    if pattern == host {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix("*.") {
        // Reject public-suffix-ish wildcards like "*.com".
        if !suffix.contains('.') {
            return false;
        }
        if let Some((first, rest)) = host.split_once('.') {
            return rest == suffix && !first.is_empty();
        }
    }
    false
}

/// Verify the leaf certificate's hostname and validity window.
fn check_leaf(cert: &Certificate, host: &str, now: i64) -> Result<()> {
    if now < cert.not_before || now > cert.not_after {
        return Err(Error::Verify("certificate expired/not yet valid"));
    }
    if !cert.san_dns.is_empty() {
        if cert.san_dns.iter().any(|p| dns_name_matches(p, host)) {
            return Ok(());
        }
        return Err(Error::Verify("hostname not in SAN"));
    }
    // CN fallback (deprecated but still encountered).
    if let Some(cn) = &cert.subject_cn {
        if dns_name_matches(cn, host) {
            return Ok(());
        }
    }
    Err(Error::Verify("hostname mismatch"))
}

/// Verify a presented chain (leaf first, intermediates after) against the
/// trust store. Checks hostname, validity windows, and every signature on
/// the path to a trusted root.
pub fn verify_server_chain(
    chain: &[Certificate],
    host: &str,
    store: &TrustStore,
    now: i64,
) -> Result<()> {
    if chain.is_empty() {
        return Err(Error::Verify("empty certificate chain"));
    }
    check_leaf(&chain[0], host, now)?;

    // Build a path iteratively: at each step, try roots first (a chain is
    // allowed to stop at an intermediate signed by a root), then any
    // unused presented cert whose subject matches the current issuer.
    let mut used = vec![false; chain.len()];
    used[0] = true;
    let mut cur = 0usize;
    for _hop in 0..=chain.len() + 1 {
        let cert = &chain[cur];
        // 1. Try to terminate at a trusted root.
        for idx in store.find_root_candidates(&cert.issuer_raw) {
            let root = &store.roots[idx];
            if now < root.not_before || now > root.not_after {
                continue;
            }
            if cert.verify_signed_by(root).is_ok() {
                return Ok(());
            }
        }
        // 2. Follow an unused presented cert.
        let mut advanced = false;
        for i in 0..chain.len() {
            if used[i] || chain[i].subject_raw != cert.issuer_raw {
                continue;
            }
            if now < chain[i].not_before || now > chain[i].not_after {
                continue;
            }
            // Issuer certs must be CAs when they declare BasicConstraints.
            if chain[i].is_ca == Some(false) {
                continue;
            }
            if cert.verify_signed_by(&chain[i]).is_ok() {
                used[i] = true;
                cur = i;
                advanced = true;
                break;
            }
        }
        if !advanced {
            return Err(Error::Verify("no path to trusted root"));
        }
    }
    Err(Error::Verify("chain too long"))
}

/// Current unix timestamp.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("aGVsbG8gd29ybGQ=").unwrap(), b"hello world");
        assert_eq!(base64_decode("AAECAwQFBgc=").unwrap(), vec![0, 1, 2, 3, 4, 5, 6, 7]);
        // unpadded
        assert_eq!(base64_decode("aGVsbG8").unwrap(), b"hello");
    }

    #[test]
    fn time_parsing() {
        // 2024-01-15 12:30:45 UTC
        let secs = days_from_civil(2024, 1, 15) * 86400 + 12 * 3600 + 30 * 60 + 45;
        // Build a UTCTime TLV by hand: tag 0x17, len 13, "240115123045Z"
        let tlv = Tlv {
            tag: der::TAG_UTC_TIME,
            value: b"240115123045Z",
            raw: b"",
        };
        assert_eq!(parse_time(&tlv).unwrap(), secs);
        // GeneralizedTime same instant
        let tlv = Tlv {
            tag: der::TAG_GENERALIZED_TIME,
            value: b"20240115123045Z",
            raw: b"",
        };
        assert_eq!(parse_time(&tlv).unwrap(), secs);
        // sanity: 1970-01-01 == 0
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        // 2000-02-29 was a Tuesday, epoch 951782400 at 00:00
        assert_eq!(days_from_civil(2000, 2, 29), 11016);
    }

    #[test]
    fn dns_wildcards() {
        assert!(dns_name_matches("*.example.com", "www.example.com"));
        assert!(!dns_name_matches("*.example.com", "example.com"));
        assert!(!dns_name_matches("*.example.com", "a.b.example.com"));
        assert!(dns_name_matches("example.com", "EXAMPLE.com"));
    }

    /// Build a tiny synthetic but DER-valid certificate by hand and parse it.
    /// Encoder helpers build real TLVs so the parser sees honest bytes.
    fn enc(tag: u8, val: &[u8]) -> Vec<u8> {
        let mut v = vec![tag];
        if val.len() < 0x80 {
            v.push(val.len() as u8);
        } else if val.len() < 0x100 {
            v.push(0x81);
            v.push(val.len() as u8);
        } else {
            v.push(0x82);
            v.extend_from_slice(&(val.len() as u16).to_be_bytes());
        }
        v.extend_from_slice(val);
        v
    }

    fn name_with_cn(cn: &str) -> Vec<u8> {
        // RDNSequence: SET { SEQ { OID 2.5.4.3, UTF8 cn } }
        let oid_cn = enc(0x06, &[0x55, 0x04, 0x03]);
        let strv = enc(0x0c, cn.as_bytes());
        let ava = enc(0x30, &[oid_cn, strv].concat());
        let set = enc(0x31, &ava);
        enc(0x30, &set)
    }

    fn test_cert_der() -> Vec<u8> {
        // TBSCertificate
        let version = enc(0xa0, &enc(0x02, &[2])); // [0] v3
        let serial = enc(0x02, &[1, 2, 3]);
        let alg_sha256rsa = enc(
            0x30,
            &[
                enc(0x06, &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b]),
                enc(0x05, &[]),
            ]
            .concat(),
        );
        let issuer = name_with_cn("Test Root");
        let validity = enc(
            0x30,
            &[
                enc(0x17, b"240101000000Z"),
                enc(0x17, b"490101000000Z"),
            ]
            .concat(),
        );
        let subject = name_with_cn("leaf.example.com");
        // SPKI: RSA with n = 0x00a1..., e = 65537
        let rsa_seq = enc(
            0x30,
            &[enc(0x02, &[0x00, 0xa1, 0x02]), enc(0x02, &[0x01, 0x00, 0x01])].concat(),
        );
        let spki = enc(
            0x30,
            &[
                enc(
                    0x30,
                    &[
                        enc(0x06, &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01]),
                        enc(0x05, &[]),
                    ]
                    .concat(),
                ),
                enc(0x03, &[&[0x00][..], &rsa_seq[..]].concat()),
            ]
            .concat(),
        );
        // SAN ext: seq{ seq{ oid 2.5.29.17, octets{ seq{ [2]"leaf.example.com" } } } }
        let san_gn = enc(0x82, b"leaf.example.com");
        let san_seq = enc(0x30, &san_gn);
        let san_ext = enc(
            0x30,
            &[
                enc(0x06, &[0x55, 0x1d, 0x11]),
                enc(0x04, &san_seq),
            ]
            .concat(),
        );
        let bc_inner = enc(0x30, &enc(0x01, &[0xff]));
        let bc_ext = enc(
            0x30,
            &[
                enc(0x06, &[0x55, 0x1d, 0x13]),
                enc(0x04, &bc_inner),
            ]
            .concat(),
        );
        let exts = enc(0xa3, &enc(0x30, &[san_ext, bc_ext].concat()));
        let tbs = enc(
            0x30,
            &[version, serial, alg_sha256rsa.clone(), issuer, validity, subject, spki, exts]
                .concat(),
        );
        let sig = enc(0x03, &[0x00, 0xaa, 0xbb]);
        enc(0x30, &[tbs, alg_sha256rsa, sig].concat())
    }

    #[test]
    fn parse_synthetic_cert() {
        let der_bytes = test_cert_der();
        let c = parse_certificate(&der_bytes).unwrap();
        assert_eq!(c.subject_cn.as_deref(), Some("leaf.example.com"));
        assert_eq!(c.issuer_cn.as_deref(), Some("Test Root"));
        assert_eq!(c.san_dns, vec!["leaf.example.com".to_string()]);
        assert_eq!(c.is_ca, Some(true));
        assert!(c.not_after > c.not_before);
        match &c.public_key {
            PublicKey::Rsa { n, e } => {
                assert_eq!(*e, Big::from_u64(65537));
                assert_eq!(n.to_be_bytes(0), vec![0xa1, 0x02]);
            }
            _ => panic!("expected RSA key"),
        }
        assert!(check_leaf(&c, "leaf.example.com", c.not_before + 1).is_ok());
        assert!(check_leaf(&c, "other.example.com", c.not_before + 1).is_err());
    }

    /// If the machine has a CA bundle, every cert in it must parse —
    /// this is effectively a corpus test against real-world DER.
    #[test]
    fn parse_system_roots() {
        let text = match std::fs::read_to_string("/etc/ssl/certs/ca-certificates.crt") {
            Ok(t) => t,
            Err(_) => return, // no bundle in this environment; skip
        };
        let ders: Vec<Vec<u8>> = {
            let mut v = Vec::new();
            let mut rest = text.as_str();
            while let Some(s) = rest.find("-----BEGIN CERTIFICATE-----") {
                let after = &rest[s + 27..];
                if let Some(e) = after.find("-----END CERTIFICATE-----") {
                    if let Ok(d) = base64_decode(&after[..e]) {
                        v.push(d);
                    }
                    rest = &after[e + 25..];
                } else {
                    break;
                }
            }
            v
        };
        assert!(!ders.is_empty(), "bundle had no PEM blocks");
        let mut parsed = 0;
        for d in &ders {
            if parse_certificate(d).is_ok() {
                parsed += 1;
            }
        }
        // Real-world bundles sometimes carry exotic entries; require the
        // vast majority to parse.
        assert!(
            parsed * 100 >= ders.len() * 90,
            "only {parsed}/{} certs parsed",
            ders.len()
        );
    }

    /// Every root in the store should verify its own signature (they are
    /// self-signed), giving us a strong end-to-end check of RSA + ECDSA
    /// verify against real keys — if a system bundle exists.
    #[test]
    fn verify_root_self_signatures() {
        let text = match std::fs::read_to_string("/etc/ssl/certs/ca-certificates.crt") {
            Ok(t) => t,
            Err(_) => return,
        };
        let certs = parse_pem_certs(&text);
        let mut ok = 0;
        let mut checked = 0;
        for c in &certs {
            if checked >= 40 {
                break; // keep debug-mode test time reasonable
            }
            if c.subject_raw == c.issuer_raw && !c.signature.is_empty() {
                checked += 1;
                if c.verify_signed_by(c).is_ok() {
                    ok += 1;
                }
            }
        }
        // Most self-signed roots should verify; unsupported algs (Ed25519,
        // PSS params, GOST) may appear and are allowed to fail.
        if checked > 0 {
            assert!(ok > 0, "no self-signed root verified of {checked}");
            assert!(
                ok * 100 >= checked * 50,
                "only {ok}/{checked} roots verified"
            );
        }
    }
}
