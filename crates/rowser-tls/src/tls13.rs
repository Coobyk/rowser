//! TLS 1.3 client (RFC 8446): record layer, handshake, and a `Read`/`Write`
//! stream over `TcpStream`.
//!
//! Supported: x25519 key share, TLS_AES_128_GCM_SHA256,
//! TLS_AES_256_GCM_SHA384, TLS_CHACHA20_POLY1305_SHA256, server-auth-only
//! (we answer a CertificateRequest with an empty Certificate). Certificate
//! verify supports RSA PKCS#1 v1.5, RSA-PSS and ECDSA P-256/P-384.
//!
//! NOT supported (fails closed): HelloRetryRequest, PSK/resumption, 0-RTT,
//! client auth with a real certificate, Ed25519, DTLS. Post-handshake
//! NewSessionTicket is read and discarded; KeyUpdate is honored.

use std::io::{self, Read, Write};
use std::net::TcpStream;

use crate::chacha20::ChaCha20Poly1305;
use crate::ecdsa;
use crate::gcm::AesGcm;
use crate::hkdf;
use crate::hmac::hmac;
use crate::rsa;
use crate::sha2::HashAlg;
use crate::x25519::PrivateKey;
use crate::x509::{self, Certificate, TrustStore};
use crate::{Error, Result};

const RT_CHANGE_CIPHER: u8 = 20;
const RT_ALERT: u8 = 21;
const RT_HANDSHAKE: u8 = 22;
const RT_APPDATA: u8 = 23;

const HS_CLIENT_HELLO: u8 = 1;
const HS_SERVER_HELLO: u8 = 2;
const HS_NEW_SESSION_TICKET: u8 = 4;
const HS_END_OF_EARLY_DATA: u8 = 5;
const HS_ENCRYPTED_EXTENSIONS: u8 = 8;
const HS_CERTIFICATE: u8 = 11;
const HS_CERTIFICATE_REQUEST: u8 = 13;
const HS_CERTIFICATE_VERIFY: u8 = 15;
const HS_FINISHED: u8 = 20;
const HS_KEY_UPDATE: u8 = 24;

/// The cipher suites we offer, in preference order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CipherSuite {
    Aes128GcmSha256,
    Aes256GcmSha384,
    ChaCha20Poly1305Sha256,
}

impl CipherSuite {
    fn from_u16(v: u16) -> Option<CipherSuite> {
        match v {
            0x1301 => Some(CipherSuite::Aes128GcmSha256),
            0x1302 => Some(CipherSuite::Aes256GcmSha384),
            0x1303 => Some(CipherSuite::ChaCha20Poly1305Sha256),
            _ => None,
        }
    }

    fn hash_alg(self) -> HashAlg {
        match self {
            CipherSuite::Aes256GcmSha384 => HashAlg::Sha384,
            _ => HashAlg::Sha256,
        }
    }

    fn key_len(self) -> usize {
        match self {
            CipherSuite::Aes128GcmSha256 => 16,
            _ => 32,
        }
    }
}

fn aead_seal(
    suite: CipherSuite,
    key: &[u8],
    nonce: &[u8; 12],
    aad: &[u8],
    pt: &[u8],
) -> (Vec<u8>, [u8; 16]) {
    match suite {
        CipherSuite::Aes128GcmSha256 => {
            AesGcm::new128(key.try_into().unwrap()).seal(nonce, aad, pt)
        }
        CipherSuite::Aes256GcmSha384 => {
            AesGcm::new256(key.try_into().unwrap()).seal(nonce, aad, pt)
        }
        CipherSuite::ChaCha20Poly1305Sha256 => {
            ChaCha20Poly1305::new(key.try_into().unwrap()).seal(nonce, aad, pt)
        }
    }
}

fn aead_open(
    suite: CipherSuite,
    key: &[u8],
    nonce: &[u8; 12],
    aad: &[u8],
    ct: &[u8],
    tag: &[u8; 16],
) -> Option<Vec<u8>> {
    match suite {
        CipherSuite::Aes128GcmSha256 => {
            AesGcm::new128(key.try_into().unwrap()).open(nonce, aad, ct, tag)
        }
        CipherSuite::Aes256GcmSha384 => {
            AesGcm::new256(key.try_into().unwrap()).open(nonce, aad, ct, tag)
        }
        CipherSuite::ChaCha20Poly1305Sha256 => {
            ChaCha20Poly1305::new(key.try_into().unwrap()).open(nonce, aad, ct, tag)
        }
    }
}

/// `static_iv XOR seq` — the per-record nonce (RFC 8446 §5.3).
fn record_nonce(iv: &[u8; 12], seq: u64) -> [u8; 12] {
    let mut n = *iv;
    for (i, b) in seq.to_be_bytes().iter().enumerate() {
        n[4 + i] ^= b;
    }
    n
}

/// Returns `(header, body)` — the header is needed as the AEAD's
/// additional data.
fn read_record(tcp: &mut TcpStream) -> Result<([u8; 5], Vec<u8>)> {
    let mut hdr = [0u8; 5];
    tcp.read_exact(&mut hdr)?;
    let len = u16::from_be_bytes([hdr[3], hdr[4]]) as usize;
    if len > (1 << 14) + 256 {
        return Err(Error::Decode("record too large"));
    }
    if len == 0 {
        return Err(Error::Decode("empty record"));
    }
    let mut body = vec![0u8; len];
    tcp.read_exact(&mut body)?;
    Ok((hdr, body))
}

fn write_record(tcp: &mut TcpStream, typ: u8, body: &[u8]) -> Result<()> {
    let mut rec = Vec::with_capacity(5 + body.len());
    rec.push(typ);
    rec.extend_from_slice(&[0x03, 0x03]);
    rec.extend_from_slice(&(body.len() as u16).to_be_bytes());
    rec.extend_from_slice(body);
    tcp.write_all(&rec)?;
    Ok(())
}

fn u24(n: usize) -> [u8; 3] {
    [(n >> 16) as u8, (n >> 8) as u8, n as u8]
}

fn handshake_msg(typ: u8, body: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(4 + body.len());
    m.push(typ);
    m.extend_from_slice(&u24(body.len()));
    m.extend_from_slice(body);
    m
}

fn ext(typ: u16, data: &[u8]) -> Vec<u8> {
    let mut e = Vec::with_capacity(4 + data.len());
    e.extend_from_slice(&typ.to_be_bytes());
    e.extend_from_slice(&(data.len() as u16).to_be_bytes());
    e.extend_from_slice(data);
    e
}

/// Build the ClientHello handshake message (4-byte header included).
fn build_client_hello(host: &str, pubkey: &[u8; 32], random: &[u8; 32], sid: &[u8; 32]) -> Vec<u8> {
    let mut exts = Vec::new();

    // server_name — only for DNS hosts, not IP literals.
    if host.parse::<std::net::IpAddr>().is_err() {
        let mut entry = vec![0x00];
        entry.extend_from_slice(&(host.len() as u16).to_be_bytes());
        entry.extend_from_slice(host.as_bytes());
        let mut list = Vec::new();
        list.extend_from_slice(&(entry.len() as u16).to_be_bytes());
        list.extend_from_slice(&entry);
        exts.extend_from_slice(&ext(0x0000, &list));
    }

    // supported_versions: TLS 1.3 only.
    exts.extend_from_slice(&ext(0x002b, &[0x02, 0x03, 0x04]));

    // supported_groups: x25519.
    exts.extend_from_slice(&ext(0x000a, &[0x00, 0x02, 0x00, 0x1d]));

    // key_share: x25519.
    let mut share = vec![0x00, 0x1d];
    share.extend_from_slice(&(pubkey.len() as u16).to_be_bytes());
    share.extend_from_slice(pubkey);
    let mut ks_list = Vec::new();
    ks_list.extend_from_slice(&(share.len() as u16).to_be_bytes());
    ks_list.extend_from_slice(&share);
    exts.extend_from_slice(&ext(0x0033, &ks_list));

    // signature_algorithms.
    let algs: [u16; 8] = [
        0x0403, // ecdsa_secp256r1_sha256
        0x0503, // ecdsa_secp384r1_sha384
        0x0804, // rsa_pss_rsae_sha256
        0x0805, // rsa_pss_rsae_sha384
        0x0806, // rsa_pss_rsae_sha512
        0x0401, // rsa_pkcs1_sha256
        0x0501, // rsa_pkcs1_sha384
        0x0601, // rsa_pkcs1_sha512
    ];
    let mut sa = Vec::new();
    sa.extend_from_slice(&((algs.len() * 2) as u16).to_be_bytes());
    for a in algs {
        sa.extend_from_slice(&a.to_be_bytes());
    }
    exts.extend_from_slice(&ext(0x000d, &sa));

    // psk_key_exchange_modes: psk_dhe_ke — required for the server to send
    // tickets; harmless to advertise since we never use the PSK itself.
    exts.extend_from_slice(&ext(0x002d, &[0x01, 0x01]));

    // legacy body
    let mut body = Vec::new();
    body.extend_from_slice(&[0x03, 0x03]); // legacy_version
    body.extend_from_slice(random);
    body.push(sid.len() as u8); // legacy_session_id (compat echo)
    body.extend_from_slice(sid);
    // cipher_suites: prefer AES-128-GCM, then ChaCha20, then AES-256-GCM.
    body.extend_from_slice(&[0x00, 0x06, 0x13, 0x01, 0x13, 0x03, 0x13, 0x02]);
    body.extend_from_slice(&[0x01, 0x00]); // compression_methods: null
    body.extend_from_slice(&(exts.len() as u16).to_be_bytes());
    body.extend_from_slice(&exts);

    handshake_msg(HS_CLIENT_HELLO, &body)
}

struct ServerHello {
    suite: CipherSuite,
    peer_pubkey: [u8; 32],
}

/// RFC 8446 §4.1.3 — ServerHello.random for HelloRetryRequest is
/// SHA-256("HelloRetryRequest"); computed rather than transcribed.
fn hrr_random() -> [u8; 32] {
    crate::sha2::sha256(b"HelloRetryRequest")
}

fn parse_server_hello(body: &[u8]) -> Result<ServerHello> {
    if body.len() < 38 {
        return Err(Error::Decode("serverhello: short"));
    }
    // legacy_version
    if body[0] != 0x03 || body[1] != 0x03 {
        return Err(Error::Decode("serverhello: bad version"));
    }
    if body[2..34] == hrr_random() {
        return Err(Error::Unsupported("hello retry request"));
    }
    let sid_len = body[34] as usize;
    if body.len() < 35 + sid_len + 3 {
        return Err(Error::Decode("serverhello: short sid"));
    }
    // session_id echo (ignored — middlebox compat only)
    let pos = 35 + sid_len;
    let suite = CipherSuite::from_u16(u16::from_be_bytes([body[pos], body[pos + 1]]))
        .ok_or(Error::Unsupported("cipher suite"))?;
    if body[pos + 2] != 0x00 {
        return Err(Error::Decode("serverhello: compression"));
    }
    let mut pos = pos + 3;
    if body.len() < pos + 2 {
        return Err(Error::Decode("serverhello: no extensions"));
    }
    let ext_len = u16::from_be_bytes([body[pos], body[pos + 1]]) as usize;
    pos += 2;
    if body.len() < pos + ext_len {
        return Err(Error::Decode("serverhello: truncated extensions"));
    }
    let exts = &body[pos..pos + ext_len];

    let mut peer_pubkey = None;
    let mut versions_ok = false;
    let mut i = 0;
    while i + 4 <= exts.len() {
        let typ = u16::from_be_bytes([exts[i], exts[i + 1]]);
        let len = u16::from_be_bytes([exts[i + 2], exts[i + 3]]) as usize;
        i += 4;
        if i + len > exts.len() {
            return Err(Error::Decode("serverhello: bad ext len"));
        }
        let data = &exts[i..i + len];
        i += len;
        match typ {
            0x002b => {
                // supported_versions: exact [0x03,0x04]
                if data == &[0x03u8, 0x04][..] {
                    versions_ok = true;
                }
            }
            0x0033 => {
                // key_share: group u16 || len u16 || key
                if data.len() < 4 {
                    return Err(Error::Decode("key_share: short"));
                }
                let group = u16::from_be_bytes([data[0], data[1]]);
                let klen = u16::from_be_bytes([data[2], data[3]]) as usize;
                if group != 0x001d || data.len() < 4 + klen || klen != 32 {
                    return Err(Error::Decode("key_share: unexpected group/len"));
                }
                let mut pk = [0u8; 32];
                pk.copy_from_slice(&data[4..4 + 32]);
                peer_pubkey = Some(pk);
            }
            0x0029 => return Err(Error::Unsupported("pre_shared_key chosen")),
            _ => {}
        }
    }
    if !versions_ok {
        return Err(Error::Decode("serverhello: no supported_versions"));
    }
    let peer_pubkey = peer_pubkey.ok_or(Error::Decode("serverhello: no key_share"))?;
    Ok(ServerHello { suite, peer_pubkey })
}

/// Parse the Certificate handshake body into a list of DER certs.
fn parse_certificate_msg(body: &[u8]) -> Result<Vec<Certificate>> {
    // request_context: u8 len + bytes; then u24 list len; entries:
    // u24 cert len + cert + u16 ext len + exts
    if body.is_empty() {
        return Err(Error::Decode("certificate: empty"));
    }
    let ctx_len = body[0] as usize;
    if body.len() < 1 + ctx_len + 3 {
        return Err(Error::Decode("certificate: short"));
    }
    let mut pos = 1 + ctx_len;
    let list_len =
        ((body[pos] as usize) << 16) | ((body[pos + 1] as usize) << 8) | body[pos + 2] as usize;
    pos += 3;
    if body.len() < pos + list_len {
        return Err(Error::Decode("certificate: list overrun"));
    }
    let end = pos + list_len;
    let mut certs = Vec::new();
    while pos < end {
        if pos + 3 > end {
            return Err(Error::Decode("certificate: entry len"));
        }
        let clen =
            ((body[pos] as usize) << 16) | ((body[pos + 1] as usize) << 8) | body[pos + 2] as usize;
        pos += 3;
        if pos + clen > end {
            return Err(Error::Decode("certificate: cert overrun"));
        }
        let der = &body[pos..pos + clen];
        pos += clen;
        certs.push(x509::parse_certificate(der)?);
        if pos + 2 > end {
            return Err(Error::Decode("certificate: ext len"));
        }
        let xlen = u16::from_be_bytes([body[pos], body[pos + 1]]) as usize;
        pos += 2 + xlen;
        if pos > end {
            return Err(Error::Decode("certificate: ext overrun"));
        }
    }
    if certs.is_empty() {
        return Err(Error::Decode("certificate: no certs"));
    }
    Ok(certs)
}

/// Verify the CertificateVerify signature over the handshake transcript.
fn verify_cert_signature(
    scheme: u16,
    sig: &[u8],
    cert: &Certificate,
    transcript_hash: &[u8],
) -> Result<()> {
    // Signed content: 64 * 0x20 || context string || 0x00 || transcript hash
    let mut content = Vec::with_capacity(64 + 33 + 1 + transcript_hash.len());
    content.extend_from_slice(&[0x20u8; 64]);
    content.extend_from_slice(b"TLS 1.3, server CertificateVerify");
    content.push(0x00);
    content.extend_from_slice(transcript_hash);

    match scheme {
        0x0401 | 0x0501 | 0x0601 => {
            let alg = match scheme {
                0x0501 => HashAlg::Sha384,
                0x0601 => HashAlg::Sha512,
                _ => HashAlg::Sha256,
            };
            let digest = alg.hash(&content);
            match &cert.public_key {
                x509::PublicKey::Rsa { n, e } => rsa::pkcs1v15_verify(n, e, alg, &digest, sig),
                _ => Err(Error::Verify("certverify: not an rsa key")),
            }
        }
        0x0804 | 0x0805 | 0x0806 => {
            let alg = match scheme {
                0x0805 => HashAlg::Sha384,
                0x0806 => HashAlg::Sha512,
                _ => HashAlg::Sha256,
            };
            let digest = alg.hash(&content);
            match &cert.public_key {
                x509::PublicKey::Rsa { n, e } => rsa::pss_verify(n, e, alg, &digest, sig),
                _ => Err(Error::Verify("certverify: not an rsa key")),
            }
        }
        0x0403 | 0x0503 => {
            let (alg, curve, key) = match scheme {
                0x0503 => (
                    HashAlg::Sha384,
                    ecdsa::p384(),
                    match &cert.public_key {
                        x509::PublicKey::EcP384(k) => k.clone(),
                        _ => return Err(Error::Verify("certverify: not p384 key")),
                    },
                ),
                _ => (
                    HashAlg::Sha256,
                    ecdsa::p256(),
                    match &cert.public_key {
                        x509::PublicKey::EcP256(k) => k.clone(),
                        _ => return Err(Error::Verify("certverify: not p256 key")),
                    },
                ),
            };
            let digest = alg.hash(&content);
            let (r, s) = decode_sig_der(sig)?;
            ecdsa::ecdsa_verify(&curve, &key, &digest, &r, &s)
        }
        _ => Err(Error::Unsupported("certverify: signature scheme")),
    }
}

/// `SEQUENCE { r INTEGER, s INTEGER }` — same shape the x509 module parses;
/// duplicated small helper via x509 to keep DER in one place.
fn decode_sig_der(sig: &[u8]) -> Result<(crate::bignum::Big, crate::bignum::Big)> {
    crate::x509::decode_signature_rs(sig)
}

/// A symmetric traffic key set for one direction.
#[derive(Default)]
struct TrafficKeys {
    secret: Vec<u8>,
    key: Vec<u8>,
    iv: [u8; 12],
    seq: u64,
}

fn traffic_from_secret(suite: CipherSuite, secret: &[u8]) -> Result<TrafficKeys> {
    let alg = suite.hash_alg();
    let key = hkdf::expand_label(alg, secret, "key", &[], suite.key_len())?;
    let iv_v = hkdf::expand_label(alg, secret, "iv", &[], 12)?;
    let mut iv = [0u8; 12];
    iv.copy_from_slice(&iv_v);
    Ok(TrafficKeys {
        secret: secret.to_vec(),
        key,
        iv,
        seq: 0,
    })
}

/// A TLS 1.3 connection. Perform the handshake with [`TlsStream::connect`],
/// then use it as a blocking `Read`/`Write` stream of application data.
///
/// IMPORTANT: `connect` does not verify the certificate chain — callers
/// must call [`TlsStream::verify_certificate`] before trusting the data.
pub struct TlsStream {
    tcp: TcpStream,
    suite: CipherSuite,
    host: String,
    read_keys: TrafficKeys,
    write_keys: TrafficKeys,
    /// Decrypted app-data not yet consumed by `read`.
    plain: Vec<u8>,
    plain_pos: usize,
    /// Handshake bytes reassembly buffer (post-handshake messages).
    hs_buf: Vec<u8>,
    peer_certs: Vec<Certificate>,
    eof: bool,
}

impl TlsStream {
    /// Perform a TLS 1.3 handshake over `tcp` for `host:port`.
    /// `host` goes into SNI (unless it is an IP literal).
    pub fn connect(host: &str, port: u16, tcp: TcpStream) -> io::Result<TlsStream> {
        let _ = port;
        handshake(host, tcp).map_err(io::Error::from)
    }

    /// Peer certificates, leaf first, as presented during the handshake.
    pub fn peer_certificates(&self) -> &[Certificate] {
        &self.peer_certs
    }

    /// Verify the peer's certificate chain against the system root store
    /// (`/etc/ssl/certs`), including hostname and validity checks.
    pub fn verify_certificate(&self) -> Result<()> {
        let store = TrustStore::system();
        if store.roots.is_empty() {
            return Err(Error::Verify("no system roots found"));
        }
        x509::verify_server_chain(&self.peer_certs, &self.host, &store, x509::now_unix())
    }

    /// Verify against an explicit root store instead of the system one.
    pub fn verify_certificate_with(&self, store: &TrustStore, now: i64) -> Result<()> {
        x509::verify_server_chain(&self.peer_certs, &self.host, store, now)
    }

    /// Encrypt `data` as one app-data record and send it.
    fn send_appdata(&mut self, data: &[u8]) -> Result<()> {
        let nonce = record_nonce(&self.write_keys.iv, self.write_keys.seq);
        let mut inner = Vec::with_capacity(data.len() + 1);
        inner.extend_from_slice(data);
        inner.push(RT_APPDATA);
        let mut aad = [0u8; 5];
        aad[0] = RT_APPDATA;
        aad[1] = 0x03;
        aad[2] = 0x03;
        // length = inner + tag
        aad[3..5].copy_from_slice(&((inner.len() + 16) as u16).to_be_bytes());
        let (ct, tag) = aead_seal(self.suite, &self.write_keys.key, &nonce, &aad, &inner);
        let mut rec = ct;
        rec.extend_from_slice(&tag);
        self.write_keys.seq += 1;
        write_record(&mut self.tcp, RT_APPDATA, &rec)
    }

    /// Encrypt a handshake message (post-handshake, e.g. KeyUpdate reply)
    /// with the current app write keys.
    fn send_encrypted_handshake(&mut self, msg: &[u8]) -> Result<()> {
        let nonce = record_nonce(&self.write_keys.iv, self.write_keys.seq);
        let mut inner = Vec::with_capacity(msg.len() + 1);
        inner.extend_from_slice(msg);
        inner.push(RT_HANDSHAKE);
        let mut aad = [0u8; 5];
        aad[0] = RT_APPDATA;
        aad[1] = 0x03;
        aad[2] = 0x03;
        aad[3..5].copy_from_slice(&((inner.len() + 16) as u16).to_be_bytes());
        let (ct, tag) = aead_seal(self.suite, &self.write_keys.key, &nonce, &aad, &inner);
        let mut rec = ct;
        rec.extend_from_slice(&tag);
        self.write_keys.seq += 1;
        write_record(&mut self.tcp, RT_APPDATA, &rec)
    }

    /// Pull one record, decrypt, and dispatch. App data goes into
    /// `self.plain`; post-handshake messages are handled in place.
    /// Returns Ok(true) when app data or EOF arrived.
    fn pump(&mut self) -> Result<()> {
        loop {
            let (hdr, body) = match read_record(&mut self.tcp) {
                Ok(rb) => rb,
                // Servers (Google among them) close the TCP connection after
                // the last record without close_notify — a clean end of the
                // response, not a protocol error.
                Err(Error::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    self.eof = true;
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
            match hdr[0] {
                RT_CHANGE_CIPHER => continue, // ignore compat CCS
                RT_ALERT => {
                    if body.len() >= 2 && body[1] == 0 {
                        self.eof = true;
                        return Ok(());
                    }
                    return Err(Error::Alert(body.get(1).copied().unwrap_or(255)));
                }
                RT_APPDATA => {}
                _ => return Err(Error::Decode("unexpected record type")),
            }
            // The AAD is the record's own header.
            let aad = hdr;
            if body.len() < 16 {
                return Err(Error::Decode("short ciphertext"));
            }
            let (ct, tag) = body.split_at(body.len() - 16);
            let tag: &[u8; 16] = tag.try_into().unwrap();
            let nonce = record_nonce(&self.read_keys.iv, self.read_keys.seq);
            let inner = aead_open(self.suite, &self.read_keys.key, &nonce, &aad, ct, tag)
                .ok_or(Error::Verify("record decryption failed"))?;
            self.read_keys.seq += 1;

            // Strip TLSInnerPlaintext: content || type || zeros
            let mut end = inner.len();
            while end > 0 && inner[end - 1] == 0 {
                end -= 1;
            }
            if end == 0 {
                return Err(Error::Decode("no inner content type"));
            }
            let inner_type = inner[end - 1];
            let content = &inner[..end - 1];
            match inner_type {
                RT_APPDATA => {
                    if content.is_empty() {
                        continue; // padding-only record
                    }
                    self.plain = content.to_vec();
                    self.plain_pos = 0;
                    return Ok(());
                }
                RT_HANDSHAKE => {
                    self.hs_buf.extend_from_slice(content);
                    self.process_post_handshake()?;
                    // continue reading
                }
                RT_ALERT => {
                    if content.len() >= 2 && content[1] == 0 {
                        self.eof = true;
                        return Ok(());
                    }
                    if content.len() >= 2 && content[0] == 2 {
                        return Err(Error::Alert(content[1]));
                    }
                    // warning: ignore
                }
                _ => return Err(Error::Decode("bad inner type")),
            }
        }
    }

    /// Handle complete post-handshake handshake messages in `hs_buf`.
    fn process_post_handshake(&mut self) -> Result<()> {
        loop {
            if self.hs_buf.len() < 4 {
                return Ok(());
            }
            let len = ((self.hs_buf[1] as usize) << 16)
                | ((self.hs_buf[2] as usize) << 8)
                | self.hs_buf[3] as usize;
            if self.hs_buf.len() < 4 + len {
                return Ok(());
            }
            let msg: Vec<u8> = self.hs_buf.drain(..4 + len).collect();
            match msg[0] {
                HS_NEW_SESSION_TICKET => { /* ignore */ }
                HS_KEY_UPDATE => {
                    if len < 1 {
                        return Err(Error::Decode("key_update: empty"));
                    }
                    let request = msg[4];
                    // Update peer's sending (our reading) keys.
                    self.update_read_secret()?;
                    if request == 1 {
                        // Must send our own KeyUpdate before updating write keys.
                        let reply = handshake_msg(HS_KEY_UPDATE, &[0x00]);
                        self.send_encrypted_handshake(&reply)?;
                        self.update_write_secret()?;
                    }
                }
                _ => { /* unknown post-handshake msg: ignore */ }
            }
        }
    }

    fn next_secret(&self, secret: &[u8]) -> Result<Vec<u8>> {
        hkdf::expand_label(
            self.suite.hash_alg(),
            secret,
            "traffic upd",
            &[],
            self.suite.hash_alg().hash_len(),
        )
    }

    fn update_read_secret(&mut self) -> Result<()> {
        let s = self.next_secret(&self.read_keys.secret)?;
        self.read_keys = traffic_from_secret(self.suite, &s)?;
        Ok(())
    }

    fn update_write_secret(&mut self) -> Result<()> {
        let s = self.next_secret(&self.write_keys.secret)?;
        self.write_keys = traffic_from_secret(self.suite, &s)?;
        Ok(())
    }
}

impl Read for TlsStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.eof {
            return Ok(0);
        }
        if self.plain_pos >= self.plain.len() {
            self.pump().map_err(io::Error::from)?;
            if self.eof {
                return Ok(0);
            }
        }
        let n = (self.plain.len() - self.plain_pos).min(buf.len());
        buf[..n].copy_from_slice(&self.plain[self.plain_pos..self.plain_pos + n]);
        self.plain_pos += n;
        if self.plain_pos == self.plain.len() {
            self.plain.clear();
            self.plain_pos = 0;
        }
        Ok(n)
    }
}

impl Write for TlsStream {
    fn write(&mut self, mut data: &[u8]) -> io::Result<usize> {
        let total = data.len();
        while !data.is_empty() {
            let n = data.len().min(1 << 14);
            self.send_appdata(&data[..n]).map_err(io::Error::from)?;
            data = &data[n..];
        }
        Ok(total)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.tcp.flush()
    }
}

/// The whole handshake: returns a connected stream on success.
fn handshake(host: &str, mut tcp: TcpStream) -> Result<TlsStream> {
    let priv_key = PrivateKey::generate();
    let pub_key = priv_key.public_key();

    let mut random = [0u8; 32];
    let mut sid = [0u8; 32];
    crate::rand_bytes(&mut random);
    crate::rand_bytes(&mut sid);

    let ch = build_client_hello(host, &pub_key.0, &random, &sid);
    // Send as a plaintext handshake record (legacy version 0x0301).
    {
        let mut rec = Vec::with_capacity(5 + ch.len());
        rec.push(RT_HANDSHAKE);
        rec.extend_from_slice(&[0x03, 0x01]);
        rec.extend_from_slice(&(ch.len() as u16).to_be_bytes());
        rec.extend_from_slice(&ch);
        tcp.write_all(&rec)?;
    }

    let mut transcript: Vec<u8> = ch.clone();
    let hash_of = |alg: HashAlg, t: &[u8]| alg.hash(t);

    // ---- read ServerHello (plaintext record; skip CCS; messages may be
    // fragmented across records so reassemble) ----
    let mut sh_buf: Vec<u8> = Vec::new();
    let sh_body = loop {
        let (hdr, body) = read_record(&mut tcp)?;
        match hdr[0] {
            RT_CHANGE_CIPHER => continue,
            RT_ALERT => return Err(Error::Alert(*body.get(1).unwrap_or(&255))),
            RT_HANDSHAKE => sh_buf.extend_from_slice(&body),
            _ => return Err(Error::Decode("unexpected record")),
        }
        if sh_buf.len() >= 4 {
            let len =
                ((sh_buf[1] as usize) << 16) | ((sh_buf[2] as usize) << 8) | sh_buf[3] as usize;
            if sh_buf.len() >= 4 + len {
                if sh_buf[0] != HS_SERVER_HELLO {
                    return Err(Error::Decode("expected server hello"));
                }
                if sh_buf.len() > 4 + len {
                    return Err(Error::Decode("extra data after server hello"));
                }
                transcript.extend_from_slice(&sh_buf);
                break sh_buf[4..4 + len].to_vec();
            }
        }
    };
    let sh = parse_server_hello(&sh_body)?;
    let suite = sh.suite;
    let alg = suite.hash_alg();

    // ---- key schedule ----
    let shared = priv_key.dh(&crate::x25519::PublicKey(sh.peer_pubkey));
    if shared.iter().all(|&b| b == 0) {
        return Err(Error::Verify("x25519 all-zero shared secret"));
    }
    let zeros = vec![0u8; alg.hash_len()];
    let early = hkdf::extract(alg, &[], &zeros);
    let empty_hash = hash_of(alg, &[]);
    let derived = hkdf::derive_secret(alg, &early, "derived", &empty_hash)?;
    let hs_secret = hkdf::extract(alg, &derived, &shared);

    let th = hash_of(alg, &transcript); // CH .. SH
    let c_hs_secret = hkdf::derive_secret(alg, &hs_secret, "c hs traffic", &th)?;
    let s_hs_secret = hkdf::derive_secret(alg, &hs_secret, "s hs traffic", &th)?;

    let mut rkeys = traffic_from_secret(suite, &s_hs_secret)?;
    let mut wkeys = traffic_from_secret(suite, &c_hs_secret)?;

    let fin_key_s = hkdf::expand_label(alg, &s_hs_secret, "finished", &[], alg.hash_len())?;
    let fin_key_c = hkdf::expand_label(alg, &c_hs_secret, "finished", &[], alg.hash_len())?;

    // ---- read encrypted handshake flight ----
    let mut hsbuf: Vec<u8> = Vec::new();
    let mut peer_certs: Vec<Certificate> = Vec::new();
    let mut cert_req_ctx: Option<Vec<u8>> = None;
    let mut server_finished = false;

    while !server_finished {
        let (hdr, body) = read_record(&mut tcp)?;
        match hdr[0] {
            RT_CHANGE_CIPHER => continue,
            RT_ALERT => return Err(Error::Alert(*body.get(1).unwrap_or(&255))),
            RT_APPDATA => {}
            RT_HANDSHAKE => {
                // Extra plaintext handshake data after SH shouldn't happen;
                // treat as fatal.
                return Err(Error::Decode("plaintext handshake after sh"));
            }
            _ => return Err(Error::Decode("unexpected record")),
        }

        // decrypt; AAD is the record header itself
        let aad = hdr;
        if body.len() < 16 {
            return Err(Error::Decode("short ciphertext"));
        }
        let (ct, tag) = body.split_at(body.len() - 16);
        let tag: &[u8; 16] = tag.try_into().unwrap();
        let nonce = record_nonce(&rkeys.iv, rkeys.seq);
        let inner = aead_open(suite, &rkeys.key, &nonce, &aad, ct, tag)
            .ok_or(Error::Verify("handshake record decrypt failed"))?;
        rkeys.seq += 1;

        let mut end = inner.len();
        while end > 0 && inner[end - 1] == 0 {
            end -= 1;
        }
        if end == 0 {
            return Err(Error::Decode("empty inner plaintext"));
        }
        let inner_type = inner[end - 1];
        match inner_type {
            RT_HANDSHAKE => hsbuf.extend_from_slice(&inner[..end - 1]),
            RT_ALERT => {
                let c = &inner[..end - 1];
                return Err(Error::Alert(*c.get(1).unwrap_or(&255)));
            }
            _ => return Err(Error::Decode("unexpected inner type in handshake")),
        }

        // Process all complete handshake messages now buffered.
        loop {
            if hsbuf.len() < 4 {
                break;
            }
            let mlen = ((hsbuf[1] as usize) << 16) | ((hsbuf[2] as usize) << 8) | hsbuf[3] as usize;
            if hsbuf.len() < 4 + mlen {
                break;
            }
            let msg: Vec<u8> = hsbuf.drain(..4 + mlen).collect();
            let mtype = msg[0];
            let mbody = &msg[4..];
            match mtype {
                HS_ENCRYPTED_EXTENSIONS => {
                    transcript.extend_from_slice(&msg);
                }
                HS_CERTIFICATE_REQUEST => {
                    // Capture the request context to echo in our (empty)
                    // Certificate response.
                    if !mbody.is_empty() {
                        let cl = mbody[0] as usize;
                        if mbody.len() >= 1 + cl {
                            cert_req_ctx = Some(mbody[1..1 + cl].to_vec());
                        }
                    }
                    if cert_req_ctx.is_none() {
                        cert_req_ctx = Some(Vec::new());
                    }
                    transcript.extend_from_slice(&msg);
                }
                HS_CERTIFICATE => {
                    peer_certs = parse_certificate_msg(mbody)?;
                    transcript.extend_from_slice(&msg);
                }
                HS_CERTIFICATE_VERIFY => {
                    if mbody.len() < 4 {
                        return Err(Error::Decode("certverify: short"));
                    }
                    let scheme = u16::from_be_bytes([mbody[0], mbody[1]]);
                    let slen = u16::from_be_bytes([mbody[2], mbody[3]]) as usize;
                    if mbody.len() < 4 + slen {
                        return Err(Error::Decode("certverify: sig overrun"));
                    }
                    let sig = &mbody[4..4 + slen];
                    let leaf = peer_certs
                        .first()
                        .ok_or(Error::Decode("certverify before certificate"))?;
                    // Transcript hash covers msgs BEFORE this CV.
                    let th = hash_of(alg, &transcript);
                    verify_cert_signature(scheme, sig, leaf, &th)?;
                    transcript.extend_from_slice(&msg);
                }
                HS_FINISHED => {
                    let expected = hmac(alg, &fin_key_s, &hash_of(alg, &transcript));
                    if mbody != expected.as_slice() {
                        return Err(Error::Verify("server finished mismatch"));
                    }
                    transcript.extend_from_slice(&msg);
                    server_finished = true;
                }
                _ => {
                    // Unknown handshake msg: record and ignore.
                    transcript.extend_from_slice(&msg);
                }
            }
        }
    }

    // ---- master + application secrets ----
    let derived2 = hkdf::derive_secret(alg, &hs_secret, "derived", &empty_hash)?;
    let master = hkdf::extract(alg, &derived2, &zeros);
    let th = hash_of(alg, &transcript); // CH .. server Finished
    let c_ap = hkdf::derive_secret(alg, &master, "c ap traffic", &th)?;
    let s_ap = hkdf::derive_secret(alg, &master, "s ap traffic", &th)?;

    // ---- client second flight ----
    let mut out_msgs: Vec<u8> = Vec::new();
    if let Some(ctx) = cert_req_ctx {
        // Empty Certificate response: echoed ctx, empty cert list.
        let mut cbody = Vec::new();
        cbody.push(ctx.len() as u8);
        cbody.extend_from_slice(&ctx);
        cbody.extend_from_slice(&[0x00, 0x00, 0x00]);
        let m = handshake_msg(HS_CERTIFICATE, &cbody);
        transcript.extend_from_slice(&m);
        out_msgs.extend_from_slice(&m);
    }
    let fin_msg = {
        let vd = hmac(alg, &fin_key_c, &hash_of(alg, &transcript));
        handshake_msg(HS_FINISHED, &vd)
    };
    out_msgs.extend_from_slice(&fin_msg);

    // Send a compat CCS then the encrypted client flight.
    write_record(&mut tcp, RT_CHANGE_CIPHER, &[0x01])?;
    {
        let nonce = record_nonce(&wkeys.iv, wkeys.seq);
        let mut inner = out_msgs.clone();
        inner.push(RT_HANDSHAKE);
        let mut aad = [0u8; 5];
        aad[0] = RT_APPDATA;
        aad[1] = 0x03;
        aad[2] = 0x03;
        aad[3..5].copy_from_slice(&((inner.len() + 16) as u16).to_be_bytes());
        let (ct, tag) = aead_seal(suite, &wkeys.key, &nonce, &aad, &inner);
        let mut rec = ct;
        rec.extend_from_slice(&tag);
        wkeys.seq += 1;
        write_record(&mut tcp, RT_APPDATA, &rec)?;
    }

    // Switch to application traffic keys.
    let read_keys = traffic_from_secret(suite, &s_ap)?;
    let write_keys = traffic_from_secret(suite, &c_ap)?;

    Ok(TlsStream {
        tcp,
        suite,
        host: host.to_string(),
        read_keys,
        write_keys,
        plain: Vec::new(),
        plain_pos: 0,
        // Any handshake bytes that arrived past Finished (e.g. a coalesced
        // NewSessionTicket) get processed on the first pump.
        hs_buf: hsbuf,
        peer_certs,
        eof: false,
    })
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

    #[test]
    fn nonce_xor() {
        let iv = [0xffu8; 12];
        let n = record_nonce(&iv, 1);
        assert_eq!(&n[..11], &[0xff; 11]);
        assert_eq!(n[11], 0xfe);
    }

    #[test]
    fn parse_sh_synthetic() {
        // Build a minimal valid ServerHello body.
        let mut body = vec![0x03, 0x03];
        body.extend_from_slice(&[0x11; 32]); // random
        body.push(0x00); // session_id echo: empty
        body.extend_from_slice(&[0x13, 0x01]); // TLS_AES_128_GCM_SHA256
        body.push(0x00); // compression
                         // extensions: supported_versions + key_share
        let mut exts = Vec::new();
        exts.extend_from_slice(&[0x00, 0x2b, 0x00, 0x02, 0x03, 0x04]);
        let mut ks = vec![0x00, 0x33, 0x00, 0x24];
        ks.extend_from_slice(&[0x00, 0x1d, 0x00, 0x20]);
        ks.extend_from_slice(&[0x42; 32]);
        exts.extend_from_slice(&ks);
        body.extend_from_slice(&(exts.len() as u16).to_be_bytes());
        body.extend_from_slice(&exts);
        let sh = parse_server_hello(&body).unwrap();
        assert_eq!(sh.suite, CipherSuite::Aes128GcmSha256);
        assert_eq!(sh.peer_pubkey, [0x42; 32]);
    }

    /// Live handshake against a real host. Requires network; ignored by
    /// default so `cargo test` stays green offline. Run explicitly with:
    /// `cargo test -p rowser-tls -- --ignored live`
    #[test]
    #[ignore]
    fn live_handshake_example_com() {
        use std::net::ToSocketAddrs;
        let addr = "example.com:443".to_socket_addrs().unwrap().next().unwrap();
        let tcp = TcpStream::connect(addr).unwrap();
        let mut tls = TlsStream::connect("example.com", 443, tcp).unwrap();
        tls.verify_certificate().unwrap();
        tls.write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match tls.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) => panic!("read error: {e}"),
            }
        }
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("HTTP/1.1"), "no http status in response");
        let _ = hex("00"); // silence unused helper
    }
}
