//! AES-128 and AES-256 block ciphers (FIPS 197), encrypt direction only —
//! GCM never needs block decryption.
//!
//! The S-box is generated at runtime from the GF(2^8) definition
//! (multiplicative inverse + affine transform) instead of a transcribed
//! table.

use std::sync::OnceLock;

/// Multiply in GF(2^8) modulo the AES polynomial x^8+x^4+x^3+x+1.
fn gf8_mul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    for _ in 0..8 {
        if b & 1 != 0 {
            p ^= a;
        }
        let hi = a & 0x80;
        a <<= 1;
        if hi != 0 {
            a ^= 0x1b;
        }
        b >>= 1;
    }
    p
}

/// Multiplicative inverse in GF(2^8): a^254 (with 0 -> 0).
fn gf8_inv(a: u8) -> u8 {
    if a == 0 {
        return 0;
    }
    let mut r = 1u8;
    let mut base = a;
    let mut e = 254u32;
    while e > 0 {
        if e & 1 != 0 {
            r = gf8_mul(r, base);
        }
        base = gf8_mul(base, base);
        e >>= 1;
    }
    r
}

fn sbox() -> &'static [u8; 256] {
    static SBOX: OnceLock<[u8; 256]> = OnceLock::new();
    SBOX.get_or_init(|| {
        let mut s = [0u8; 256];
        for (i, v) in s.iter_mut().enumerate() {
            let x = gf8_inv(i as u8);
            *v = x
                ^ x.rotate_left(1)
                ^ x.rotate_left(2)
                ^ x.rotate_left(3)
                ^ x.rotate_left(4)
                ^ 0x63;
        }
        s
    })
}

const RCON: [u8; 10] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36];

/// An AES key schedule. Use [`Aes::new128`] or [`Aes::new256`].
pub struct Aes {
    /// Round keys: 16 bytes each, `rounds + 1` of them.
    round_keys: Vec<u8>,
    rounds: usize,
}

impl Aes {
    pub fn new128(key: &[u8; 16]) -> Aes {
        Self::expand(key)
    }

    pub fn new256(key: &[u8; 32]) -> Aes {
        Self::expand(key)
    }

    fn expand(key: &[u8]) -> Aes {
        let s = sbox();
        let nk = key.len() / 4; // 4 or 8
        let rounds = nk + 6; // 10 or 14
        let total_words = 4 * (rounds + 1);
        let mut w = vec![0u8; 4 * total_words];
        w[..key.len()].copy_from_slice(key);
        for i in nk..total_words {
            let mut t = [
                w[4 * (i - 1)],
                w[4 * (i - 1) + 1],
                w[4 * (i - 1) + 2],
                w[4 * (i - 1) + 3],
            ];
            if i % nk == 0 {
                t = [
                    s[t[1] as usize] ^ RCON[i / nk - 1],
                    s[t[2] as usize],
                    s[t[3] as usize],
                    s[t[0] as usize],
                ];
            } else if nk > 6 && i % nk == 4 {
                t = [
                    s[t[0] as usize],
                    s[t[1] as usize],
                    s[t[2] as usize],
                    s[t[3] as usize],
                ];
            }
            for j in 0..4 {
                w[4 * i + j] = w[4 * (i - nk) + j] ^ t[j];
            }
        }
        Aes {
            round_keys: w,
            rounds,
        }
    }

    /// Encrypt one 16-byte block in place.
    pub fn encrypt_block(&self, block: &mut [u8; 16]) {
        let s = sbox();
        let mut st = *block;
        add_round_key(&mut st, &self.round_keys[0..16]);
        for round in 1..self.rounds {
            sub_bytes(&mut st, s);
            shift_rows(&mut st);
            mix_columns(&mut st);
            add_round_key(&mut st, &self.round_keys[16 * round..16 * round + 16]);
        }
        sub_bytes(&mut st, s);
        shift_rows(&mut st);
        add_round_key(&mut st, &self.round_keys[16 * self.rounds..]);
        *block = st;
    }
}

fn add_round_key(st: &mut [u8; 16], rk: &[u8]) {
    for i in 0..16 {
        st[i] ^= rk[i];
    }
}

fn sub_bytes(st: &mut [u8; 16], s: &[u8; 256]) {
    for b in st.iter_mut() {
        *b = s[*b as usize];
    }
}

/// State is column-major: `st[4*c + r]`. Row r rotates left by r.
fn shift_rows(st: &mut [u8; 16]) {
    let old = *st;
    for r in 0..4 {
        for c in 0..4 {
            st[4 * c + r] = old[4 * ((c + r) % 4) + r];
        }
    }
}

fn xtime(a: u8) -> u8 {
    let hi = a & 0x80;
    let mut r = a << 1;
    if hi != 0 {
        r ^= 0x1b;
    }
    r
}

fn mix_columns(st: &mut [u8; 16]) {
    for c in 0..4 {
        let i = 4 * c;
        let (a0, a1, a2, a3) = (st[i], st[i + 1], st[i + 2], st[i + 3]);
        let t = a0 ^ a1 ^ a2 ^ a3;
        st[i] = a0 ^ t ^ xtime(a0 ^ a1);
        st[i + 1] = a1 ^ t ^ xtime(a1 ^ a2);
        st[i + 2] = a2 ^ t ^ xtime(a2 ^ a3);
        st[i + 3] = a3 ^ t ^ xtime(a3 ^ a0);
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

    #[test]
    fn fips197_aes128() {
        // FIPS-197 Appendix B / C.1 example.
        let key: [u8; 16] = hex("000102030405060708090a0b0c0d0e0f").try_into().unwrap();
        let mut block: [u8; 16] = hex("00112233445566778899aabbccddeeff").try_into().unwrap();
        Aes::new128(&key).encrypt_block(&mut block);
        assert_eq!(hexify(&block), "69c4e0d86a7b0430d8cdb78070b4c55a");
    }

    #[test]
    fn fips197_aes256() {
        // FIPS-197 Appendix C.3.
        let key: [u8; 32] = hex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f")
            .try_into()
            .unwrap();
        let mut block: [u8; 16] = hex("00112233445566778899aabbccddeeff").try_into().unwrap();
        Aes::new256(&key).encrypt_block(&mut block);
        assert_eq!(hexify(&block), "8ea2b7ca516745bfeafc49904b496089");
    }

    #[test]
    fn sbox_spot_check() {
        // A few well-known S-box entries.
        let s = sbox();
        assert_eq!(s[0x00], 0x63);
        assert_eq!(s[0x53], 0xed);
        assert_eq!(s[0x01], 0x7c);
        assert_eq!(s[0xff], 0x16);
    }
}
