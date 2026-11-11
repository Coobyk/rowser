//! ChaCha20, Poly1305, and the ChaCha20-Poly1305 AEAD (RFC 8439).

fn quarter_round(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// One 64-byte ChaCha20 block.
pub fn block(key: &[u8; 32], counter: u32, nonce: &[u8; 12]) -> [u8; 64] {
    let mut s = [0u32; 16];
    s[0] = 0x61707865;
    s[1] = 0x3320646e;
    s[2] = 0x79622d32;
    s[3] = 0x6b206574;
    for i in 0..8 {
        s[4 + i] = le32(&key[4 * i..]);
    }
    s[12] = counter;
    for i in 0..3 {
        s[13 + i] = le32(&nonce[4 * i..]);
    }
    let init = s;
    for _ in 0..10 {
        // column rounds
        quarter_round(&mut s, 0, 4, 8, 12);
        quarter_round(&mut s, 1, 5, 9, 13);
        quarter_round(&mut s, 2, 6, 10, 14);
        quarter_round(&mut s, 3, 7, 11, 15);
        // diagonal rounds
        quarter_round(&mut s, 0, 5, 10, 15);
        quarter_round(&mut s, 1, 6, 11, 12);
        quarter_round(&mut s, 2, 7, 8, 13);
        quarter_round(&mut s, 3, 4, 9, 14);
    }
    let mut out = [0u8; 64];
    for i in 0..16 {
        out[4 * i..4 * i + 4].copy_from_slice(&s[i].wrapping_add(init[i]).to_le_bytes());
    }
    out
}

/// ChaCha20 stream XOR, starting at `initial_counter`.
pub fn xor(key: &[u8; 32], initial_counter: u32, nonce: &[u8; 12], data: &mut [u8]) {
    let mut counter = initial_counter;
    let mut offset = 0;
    while offset < data.len() {
        let ks = block(key, counter, nonce);
        let n = (data.len() - offset).min(64);
        for i in 0..n {
            data[offset + i] ^= ks[i];
        }
        counter = counter.wrapping_add(1);
        offset += n;
    }
}

/// Poly1305 one-time authenticator (RFC 8439 §2.5). `key` is 32 bytes:
/// `r` (clamped) || `s`.
pub fn poly1305(key: &[u8; 32], msg: &[u8]) -> [u8; 16] {
    // Decode r as five 26-bit limbs, clamped.
    let t0 = u32::from_le_bytes(key[0..4].try_into().unwrap()) as u64;
    let t1 = u32::from_le_bytes(key[4..8].try_into().unwrap()) as u64;
    let t2 = u32::from_le_bytes(key[8..12].try_into().unwrap()) as u64;
    let t3 = u32::from_le_bytes(key[12..16].try_into().unwrap()) as u64;
    let m = (1u64 << 26) - 1;
    let r0 = t0 & 0x3ffffff;
    let r1 = ((t0 >> 26) | (t1 << 6)) & 0x3ffff03;
    let r2 = ((t1 >> 20) | (t2 << 12)) & 0x3ffc0ff;
    let r3 = ((t2 >> 14) | (t3 << 18)) & 0x3f03fff;
    let r4 = (t3 >> 8) & 0x00fffff;
    let s1 = r1 * 5;
    let s2 = r2 * 5;
    let s3 = r3 * 5;
    let s4 = r4 * 5;

    let mut h = [0u64; 5];
    for chunk in msg.chunks(16) {
        // Each block is the message bytes plus a 1 bit at position len*8
        // (for a full block that lands in virtual byte 16).
        let mut b = [0u8; 17];
        b[..chunk.len()].copy_from_slice(chunk);
        b[chunk.len()] = 1;
        add_block(&mut h, &b, m);
        let d0 = h[0] * r0 + h[1] * s4 + h[2] * s3 + h[3] * s2 + h[4] * s1;
        let d1 = h[0] * r1 + h[1] * r0 + h[2] * s4 + h[3] * s3 + h[4] * s2;
        let d2 = h[0] * r2 + h[1] * r1 + h[2] * r0 + h[3] * s4 + h[4] * s3;
        let d3 = h[0] * r3 + h[1] * r2 + h[2] * r1 + h[3] * r0 + h[4] * s4;
        let d4 = h[0] * r4 + h[1] * r3 + h[2] * r2 + h[3] * r1 + h[4] * r0;
        // propagate carries
        let mut c = d0 >> 26;
        h[0] = d0 & m;
        let d1 = d1 + c;
        c = d1 >> 26;
        h[1] = d1 & m;
        let d2 = d2 + c;
        c = d2 >> 26;
        h[2] = d2 & m;
        let d3 = d3 + c;
        c = d3 >> 26;
        h[3] = d3 & m;
        let d4 = d4 + c;
        c = d4 >> 26;
        h[4] = d4 & m;
        h[0] += c * 5;
        c = h[0] >> 26;
        h[0] &= m;
        h[1] += c;
    }

    // Full carry, then h = min(h, h - p) where p = 2^130 - 5.
    let mut c = h[1] >> 26;
    h[1] &= m;
    h[2] += c;
    c = h[2] >> 26;
    h[2] &= m;
    h[3] += c;
    c = h[3] >> 26;
    h[3] &= m;
    h[4] += c;
    c = h[4] >> 26;
    h[4] &= m;
    h[0] += c * 5;
    c = h[0] >> 26;
    h[0] &= m;
    h[1] += c;

    // g = h - p: g_i = h_i + (-p_i) computed as h + 5 - 2^130.
    let mut g = [0u64; 5];
    g[0] = h[0] + 5;
    c = g[0] >> 26;
    g[0] &= m;
    g[1] = h[1] + c;
    c = g[1] >> 26;
    g[1] &= m;
    g[2] = h[2] + c;
    c = g[2] >> 26;
    g[2] &= m;
    g[3] = h[3] + c;
    c = g[3] >> 26;
    g[3] &= m;
    g[4] = h[4].wrapping_add(c).wrapping_sub(1 << 26);
    // If the subtraction borrowed, g4 underflowed (top bits set) => h < p:
    // keep h. Otherwise keep g.
    let select_g = (g[4] >> 63).wrapping_sub(1); // all-ones when g4 sign bit clear
    for i in 0..5 {
        h[i] = (h[i] & !select_g) | (g[i] & select_g);
    }

    // Serialize h (130 bits) into four 32-bit words, then add s mod 2^128.
    let h0 = (h[0] | (h[1] << 26)) & 0xffff_ffff;
    let h1 = ((h[1] >> 6) | (h[2] << 20)) & 0xffff_ffff;
    let h2 = ((h[2] >> 12) | (h[3] << 14)) & 0xffff_ffff;
    let h3 = ((h[3] >> 18) | (h[4] << 8)) & 0xffff_ffff;

    let s = [
        u32::from_le_bytes(key[16..20].try_into().unwrap()) as u64,
        u32::from_le_bytes(key[20..24].try_into().unwrap()) as u64,
        u32::from_le_bytes(key[24..28].try_into().unwrap()) as u64,
        u32::from_le_bytes(key[28..32].try_into().unwrap()) as u64,
    ];
    let f0 = h0 + s[0];
    let c = f0 >> 32;
    let f1 = h1 + s[1] + c;
    let c = f1 >> 32;
    let f2 = h2 + s[2] + c;
    let c = f2 >> 32;
    let f3 = h3 + s[3] + c;

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&(f0 as u32).to_le_bytes());
    out[4..8].copy_from_slice(&(f1 as u32).to_le_bytes());
    out[8..12].copy_from_slice(&(f2 as u32).to_le_bytes());
    out[12..16].copy_from_slice(&(f3 as u32).to_le_bytes());
    out
}

fn add_block(h: &mut [u64; 5], b: &[u8; 17], m: u64) {
    let t0 = u32::from_le_bytes(b[0..4].try_into().unwrap()) as u64;
    let t1 = u32::from_le_bytes(b[4..8].try_into().unwrap()) as u64;
    let t2 = u32::from_le_bytes(b[8..12].try_into().unwrap()) as u64;
    let t3 = u32::from_le_bytes(b[12..16].try_into().unwrap()) as u64;
    h[0] += t0 & m;
    h[1] += ((t0 >> 26) | (t1 << 6)) & m;
    h[2] += ((t1 >> 20) | (t2 << 12)) & m;
    h[3] += ((t2 >> 14) | (t3 << 18)) & m;
    h[4] += (t3 >> 8) | ((b[16] as u64) << 24);
}

/// ChaCha20-Poly1305 AEAD (RFC 8439 §2.8).
pub struct ChaCha20Poly1305 {
    key: [u8; 32],
}

impl ChaCha20Poly1305 {
    pub fn new(key: &[u8; 32]) -> Self {
        ChaCha20Poly1305 { key: *key }
    }

    fn poly_key(&self, nonce: &[u8; 12]) -> [u8; 32] {
        let b = block(&self.key, 0, nonce);
        let mut k = [0u8; 32];
        k.copy_from_slice(&b[..32]);
        k
    }

    fn mac_data(aad: &[u8], ct: &[u8]) -> Vec<u8> {
        let mut d = Vec::with_capacity(aad.len() + ct.len() + 40);
        d.extend_from_slice(aad);
        d.resize(d.len() + (16 - aad.len() % 16) % 16, 0);
        d.extend_from_slice(ct);
        d.resize(d.len() + (16 - ct.len() % 16) % 16, 0);
        d.extend_from_slice(&(aad.len() as u64).to_le_bytes());
        d.extend_from_slice(&(ct.len() as u64).to_le_bytes());
        d
    }

    pub fn seal(&self, nonce: &[u8; 12], aad: &[u8], plaintext: &[u8]) -> (Vec<u8>, [u8; 16]) {
        let mut ct = plaintext.to_vec();
        xor(&self.key, 1, nonce, &mut ct);
        let tag = poly1305(&self.poly_key(nonce), &Self::mac_data(aad, &ct));
        (ct, tag)
    }

    pub fn open(
        &self,
        nonce: &[u8; 12],
        aad: &[u8],
        ciphertext: &[u8],
        tag: &[u8; 16],
    ) -> Option<Vec<u8>> {
        let expected = poly1305(&self.poly_key(nonce), &Self::mac_data(aad, ciphertext));
        let mut diff = 0u8;
        for i in 0..16 {
            diff |= expected[i] ^ tag[i];
        }
        if diff != 0 {
            return None;
        }
        let mut pt = ciphertext.to_vec();
        xor(&self.key, 1, nonce, &mut pt);
        Some(pt)
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
    fn rfc8439_chacha_block() {
        // §2.3.2: key = 00..1f, nonce = 000000090000004a00000000, counter 1.
        let key: [u8; 32] = hex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f")
            .try_into()
            .unwrap();
        let nonce: [u8; 12] = hex("000000090000004a00000000").try_into().unwrap();
        let out = block(&key, 1, &nonce);
        assert_eq!(
            hexify(&out),
            "10f1e7e4d13b5915500fdd1fa32071c4\
             c7d1f4c733c068030422aa9ac3d46c4e\
             d2826446079faa0914c2d705d98b02a2\
             b5129cd1de164eb9cbd083e8a2503c4e"
        );
    }

    #[test]
    fn rfc8439_poly1305() {
        // §2.5.2
        let key: [u8; 32] = hex("85d6be7857556d337f4452fe42d506a80103808afb0db2fd4abff6af4149f51b")
            .try_into()
            .unwrap();
        let tag = poly1305(&key, b"Cryptographic Forum Research Group");
        assert_eq!(hexify(&tag), "a8061dc1305136c6c22b8baf0c0127a9");
    }

    #[test]
    fn rfc8439_aead() {
        // §2.8.2 full AEAD test.
        let key: [u8; 32] = hex("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f")
            .try_into()
            .unwrap();
        let nonce: [u8; 12] = hex("070000004041424344454647").try_into().unwrap();
        let aad = hex("50515253c0c1c2c3c4c5c6c7");
        let pt = b"Ladies and Gentlemen of the class of '99: If I could offer you \
                   only one tip for the future, sunscreen would be it.";
        let want_ct = hex("d31a8d34648e60db7b86afbc53ef7ec2\
             a4aded51296e08fea9e2b5a736ee62d6\
             3dbea45e8ca9671282fafb69da92728b\
             1a71de0a9e060b2905d6a5b67ecd3b36\
             92ddbd7f2d778b8c9803aee328091b58\
             fab324e4fad675945585808b4831d7bc\
             3ff4def08e4b7a9de576d26586cec64b\
             6116");
        let want_tag = hex("1ae10b594f09e26a7e902ecbd0600691");
        let a = ChaCha20Poly1305::new(&key);
        let (ct, tag) = a.seal(&nonce, &aad, pt);
        assert_eq!(ct, want_ct);
        assert_eq!(tag.to_vec(), want_tag);
        assert_eq!(a.open(&nonce, &aad, &ct, &tag).unwrap(), pt);
        assert!(a.open(&nonce, b"bad aad", &ct, &tag).is_none());
    }
}
