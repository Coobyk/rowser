//! rowser-tls: a from-scratch TLS 1.3 client.
//!
//! Everything here is implemented from scratch on top of `std` only — no
//! rustls, openssl, ring, or any other crypto crate. Where we implement a
//! primitive we follow the RFC/NIST algorithm and test against published
//! vectors (RFC 7748, RFC 6234, RFC 5869, RFC 8439, the GCM spec).
//!
//! Layout:
//!
//! - [`sha2`] — SHA-256, SHA-384, SHA-512.
//! - [`hmac`] / [`hkdf`] — keyed hashing and the TLS 1.3 key schedule.
//! - [`x25519`] — Curve25519 Diffie–Hellman (RFC 7748).
//! - [`aes`] / [`gcm`] — AES-128/256 block cipher and AES-GCM AEAD.
//! - [`chacha20`] — ChaCha20, Poly1305, ChaCha20-Poly1305 AEAD (RFC 8439).
//! - [`bignum`] — minimal big-unsigned-integer arithmetic for RSA/ECC.
//! - [`der`] / [`x509`] — DER parsing, certificate chain building and
//!   signature verification (RSA PKCS#1 v1.5, RSA-PSS, ECDSA P-256/P-384).
//! - [`tls13`] — the record layer, handshake and [`TlsStream`].

use std::fmt;
use std::io;

mod aes;
mod bignum;
mod chacha20;
mod der;
mod ecdsa;
mod gcm;
mod hkdf;
mod hmac;
mod rsa;
mod sha2;
mod tls13;
mod x25519;
mod x509;

pub use sha2::HashAlg;
pub use tls13::TlsStream;
pub use x509::{Certificate, TrustStore};

/// Error type for the whole crate. Crypto/decoding failures carry a static
/// description; I/O failures propagate [`io::Error`].
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// Malformed DER, record, or handshake message.
    Decode(&'static str),
    /// Peer sent a fatal TLS alert (carries the description byte).
    Alert(u8),
    /// Peer chose or required something we do not implement.
    Unsupported(&'static str),
    /// Authentication failed: bad signature, bad Finished, untrusted chain.
    Verify(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Decode(m) => write!(f, "decode error: {m}"),
            Error::Alert(d) => write!(f, "tls alert {d}"),
            Error::Unsupported(m) => write!(f, "unsupported: {m}"),
            Error::Verify(m) => write!(f, "verification failed: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<Error> for io::Error {
    fn from(e: Error) -> io::Error {
        match e {
            Error::Io(e) => e,
            other => io::Error::new(io::ErrorKind::InvalidData, other),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Fill `buf` with random bytes. Uses `/dev/urandom` when available and
/// falls back to hashing a counter seeded by time, pid, and an address —
/// good enough to keep working on exotic systems, bad enough that the
/// fallback is only a fallback.
fn rand_bytes(buf: &mut [u8]) {
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(buf).is_ok() {
            return;
        }
    }
    // Fallback: SHA-256 counter stream over whatever entropy we can find.
    let mut seed = Vec::new();
    if let Ok(t) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        seed.extend_from_slice(&t.as_nanos().to_le_bytes());
    }
    seed.extend_from_slice(&std::process::id().to_le_bytes());
    seed.extend_from_slice(&(buf.as_ptr() as usize).to_le_bytes());
    let mut ctr = 0u64;
    let mut written = 0;
    while written < buf.len() {
        let mut block = seed.clone();
        block.extend_from_slice(&ctr.to_le_bytes());
        let h = sha2::sha256(&block);
        let n = h.len().min(buf.len() - written);
        buf[written..written + n].copy_from_slice(&h[..n]);
        written += n;
        ctr += 1;
    }
}
