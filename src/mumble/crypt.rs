//! Mumble's UDP packet encryption: OCB2 with AES-128 (`CryptStateOCB2` in the
//! Mumble sources), including its nonce handling for lost/late/replayed
//! packets and the countermeasures against the published OCB2 attacks
//! (https://eprint.iacr.org/2019/311, section 9).
//!
//! Wire format of an encrypted packet: `[iv byte][tag 3 bytes][ciphertext]`.

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;

const BLOCK: usize = 16;
type Block = [u8; BLOCK];

/// Multiply by 2 in GF(2^128) (big-endian, polynomial x^128 + x^7 + x^2 + x + 1).
fn s2(b: &mut Block) {
    let carry = b[0] >> 7;
    for i in 0..BLOCK - 1 {
        b[i] = (b[i] << 1) | (b[i + 1] >> 7);
    }
    b[BLOCK - 1] = (b[BLOCK - 1] << 1) ^ (carry * 0x87);
}

/// Multiply by 3 in GF(2^128).
fn s3(b: &mut Block) {
    let orig = *b;
    s2(b);
    xor_in(b, &orig);
}

fn xor_in(a: &mut Block, b: &[u8]) {
    for (x, y) in a.iter_mut().zip(b) {
        *x ^= y;
    }
}

/// 128-bit little-endian counter addition (Mumble IVs count up from byte 0).
fn iv_add(iv: &Block, n: u64) -> Block {
    let mut out = *iv;
    let mut carry = n as u128;
    for b in out.iter_mut() {
        if carry == 0 {
            break;
        }
        let sum = *b as u128 + (carry & 0xFF);
        *b = sum as u8;
        carry = (carry >> 8) + (sum >> 8);
    }
    out
}

pub struct CryptState {
    aes: Aes128,
    encrypt_iv: Block,
    decrypt_iv: Block,
    decrypt_history: [u8; 256],
    pub good: u32,
    pub late: u32,
    pub lost: u32,
    pub resync: u32,
}

impl CryptState {
    /// `client_nonce` is our encrypt IV and `server_nonce` our decrypt IV, as sent in CryptSetup.
    pub fn new(key: &[u8], client_nonce: &[u8], server_nonce: &[u8]) -> Option<Self> {
        if key.len() != BLOCK || client_nonce.len() != BLOCK || server_nonce.len() != BLOCK {
            return None;
        }
        Some(Self {
            aes: Aes128::new(GenericArray::from_slice(key)),
            encrypt_iv: client_nonce.try_into().ok()?,
            decrypt_iv: server_nonce.try_into().ok()?,
            decrypt_history: [0; 256],
            good: 0,
            late: 0,
            lost: 0,
            resync: 0,
        })
    }

    pub fn encrypt_iv(&self) -> Block {
        self.encrypt_iv
    }

    /// Server-initiated resync of our decrypt IV.
    pub fn set_decrypt_iv(&mut self, iv: &[u8]) -> bool {
        match iv.try_into() {
            Ok(iv) => {
                self.decrypt_iv = iv;
                self.resync += 1;
                true
            }
            Err(_) => false,
        }
    }

    fn aes_enc(&self, b: &Block) -> Block {
        let mut g = GenericArray::clone_from_slice(b);
        self.aes.encrypt_block(&mut g);
        g.into()
    }

    fn aes_dec(&self, b: &Block) -> Block {
        let mut g = GenericArray::clone_from_slice(b);
        self.aes.decrypt_block(&mut g);
        g.into()
    }

    /// OCB2 encryption; returns (ciphertext, full tag).
    fn ocb_encrypt(&self, plain: &[u8], nonce: &Block) -> (Vec<u8>, Block) {
        let mut out = Vec::with_capacity(plain.len());
        let mut delta = self.aes_enc(nonce);
        let mut checksum = [0u8; BLOCK];
        let mut rest = plain;
        while rest.len() > BLOCK {
            let (blk, tail) = rest.split_at(BLOCK);
            // Counter-cryptanalysis: never encrypt a second-to-last block that
            // could be used to forge a tag (all-zero first 15 bytes): flip a bit.
            let flip = tail.len() <= BLOCK && blk[..BLOCK - 1].iter().all(|&b| b == 0);
            s2(&mut delta);
            let mut tmp = delta;
            xor_in(&mut tmp, blk);
            if flip {
                tmp[0] ^= 1;
            }
            let mut enc = self.aes_enc(&tmp);
            xor_in(&mut enc, &delta);
            out.extend_from_slice(&enc);
            xor_in(&mut checksum, blk);
            if flip {
                checksum[0] ^= 1;
            }
            rest = tail;
        }
        s2(&mut delta);
        let mut tmp = [0u8; BLOCK];
        tmp[BLOCK - 4..].copy_from_slice(&((rest.len() as u32) * 8).to_be_bytes());
        xor_in(&mut tmp, &delta);
        let pad = self.aes_enc(&tmp);
        let mut last = pad;
        last[..rest.len()].copy_from_slice(rest);
        xor_in(&mut checksum, &last);
        xor_in(&mut last, &pad);
        out.extend_from_slice(&last[..rest.len()]);
        s3(&mut delta);
        let mut t = delta;
        xor_in(&mut t, &checksum);
        (out, self.aes_enc(&t))
    }

    /// OCB2 decryption; returns (plaintext, full tag) or None if the attack check trips.
    fn ocb_decrypt(&self, enc: &[u8], nonce: &Block) -> Option<(Vec<u8>, Block)> {
        let mut out = Vec::with_capacity(enc.len());
        let mut delta = self.aes_enc(nonce);
        let mut checksum = [0u8; BLOCK];
        let mut rest = enc;
        while rest.len() > BLOCK {
            let (blk, tail) = rest.split_at(BLOCK);
            s2(&mut delta);
            let mut tmp = delta;
            xor_in(&mut tmp, blk);
            let mut p = self.aes_dec(&tmp);
            xor_in(&mut p, &delta);
            out.extend_from_slice(&p);
            xor_in(&mut checksum, &p);
            rest = tail;
        }
        s2(&mut delta);
        let mut tmp = [0u8; BLOCK];
        tmp[BLOCK - 4..].copy_from_slice(&((rest.len() as u32) * 8).to_be_bytes());
        xor_in(&mut tmp, &delta);
        let pad = self.aes_enc(&tmp);
        let mut last = [0u8; BLOCK];
        last[..rest.len()].copy_from_slice(rest);
        xor_in(&mut last, &pad);
        xor_in(&mut checksum, &last);
        out.extend_from_slice(&last[..rest.len()]);
        // Counter-cryptanalysis: a forged last block would equal delta ^ len.
        let forged = last[..BLOCK - 1] == delta[..BLOCK - 1];
        s3(&mut delta);
        let mut t = delta;
        xor_in(&mut t, &checksum);
        if forged {
            return None;
        }
        Some((out, self.aes_enc(&t)))
    }

    pub fn encrypt(&mut self, plain: &[u8]) -> Vec<u8> {
        for b in self.encrypt_iv.iter_mut() {
            *b = b.wrapping_add(1);
            if *b != 0 {
                break;
            }
        }
        let (enc, tag) = self.ocb_encrypt(plain, &self.encrypt_iv);
        let mut out = Vec::with_capacity(4 + enc.len());
        out.push(self.encrypt_iv[0]);
        out.extend_from_slice(&tag[..3]);
        out.extend_from_slice(&enc);
        out
    }

    pub fn decrypt(&mut self, src: &[u8]) -> Option<Vec<u8>> {
        self.decrypt_in_window(src).or_else(|| self.decrypt_after_burst_loss(src))
    }

    /// Recovery beyond Mumble's window: the packet carries only the low IV byte,
    /// so after a burst of more than ~128 lost packets the receiver cannot
    /// tell how far the sender's counter moved and every later packet fails
    /// until a server round-trip resync. Try the next few counter wraps
    /// instead (up to ~1280 lost packets). Candidates are strictly ahead of
    /// the current IV, so replays still fail; with a 24-bit tag the extra
    /// attempts keep forgery odds around 2^-22 per packet.
    fn decrypt_after_burst_loss(&mut self, src: &[u8]) -> Option<Vec<u8>> {
        const MAX_WRAPS: u64 = 4;
        if src.len() < 4 {
            return None;
        }
        let base = self.decrypt_iv;
        let forward = src[0].wrapping_sub(base[0]) as u64;
        for wraps in 0..=MAX_WRAPS {
            let delta = forward + 256 * wraps;
            if delta < 128 {
                continue; // inside the normal window, already tried
            }
            let nonce = iv_add(&base, delta);
            if let Some((plain, tag)) = self.ocb_decrypt(&src[4..], &nonce) {
                if tag[..3] == src[1..4] {
                    self.decrypt_iv = nonce;
                    self.decrypt_history[nonce[0] as usize] = nonce[1];
                    self.good += 1;
                    self.lost += (delta - 1) as u32;
                    return Some(plain);
                }
            }
        }
        None
    }

    fn decrypt_in_window(&mut self, src: &[u8]) -> Option<Vec<u8>> {
        if src.len() < 4 {
            return None;
        }
        let saved = self.decrypt_iv;
        let ivbyte = src[0];
        let mut restore = false;
        let mut late = 0;
        let mut lost: i32 = 0;
        let iv0 = self.decrypt_iv[0];

        if iv0.wrapping_add(1) == ivbyte {
            // In order, as expected.
            if ivbyte > iv0 {
                self.decrypt_iv[0] = ivbyte;
            } else if ivbyte < iv0 {
                self.decrypt_iv[0] = ivbyte;
                self.carry_up();
            } else {
                return None;
            }
        } else {
            // Out of order or a repeat.
            let mut diff = ivbyte as i32 - iv0 as i32;
            if diff > 128 {
                diff -= 256;
            } else if diff < -128 {
                diff += 256;
            }
            if ivbyte < iv0 && diff > -30 && diff < 0 {
                // Late packet, no wraparound.
                late = 1;
                lost = -1;
                self.decrypt_iv[0] = ivbyte;
                restore = true;
            } else if ivbyte > iv0 && diff > -30 && diff < 0 {
                // Late packet from before the last wraparound.
                late = 1;
                lost = -1;
                self.decrypt_iv[0] = ivbyte;
                for b in self.decrypt_iv[1..].iter_mut() {
                    let was = *b;
                    *b = b.wrapping_sub(1);
                    if was != 0 {
                        break;
                    }
                }
                restore = true;
            } else if ivbyte > iv0 && diff > 0 {
                // Lost a few packets.
                lost = ivbyte as i32 - iv0 as i32 - 1;
                self.decrypt_iv[0] = ivbyte;
            } else if ivbyte < iv0 && diff > 0 {
                // Lost a few packets across a wraparound.
                lost = 256 - iv0 as i32 + ivbyte as i32 - 1;
                self.decrypt_iv[0] = ivbyte;
                self.carry_up();
            } else {
                return None;
            }
            if self.decrypt_history[self.decrypt_iv[0] as usize] == self.decrypt_iv[1] {
                // Replay.
                self.decrypt_iv = saved;
                return None;
            }
        }

        let nonce = self.decrypt_iv;
        let ok = self.ocb_decrypt(&src[4..], &nonce).filter(|(_, tag)| tag[..3] == src[1..4]);
        let Some((plain, _)) = ok else {
            self.decrypt_iv = saved;
            return None;
        };
        self.decrypt_history[self.decrypt_iv[0] as usize] = self.decrypt_iv[1];
        if restore {
            self.decrypt_iv = saved;
        }
        self.good += 1;
        self.late += late;
        if lost > 0 {
            self.lost += lost as u32;
        } else if lost < 0 {
            self.lost = self.lost.saturating_sub(lost.unsigned_abs());
        }
        Some(plain)
    }

    fn carry_up(&mut self) {
        for b in self.decrypt_iv[1..].iter_mut() {
            *b = b.wrapping_add(1);
            if *b != 0 {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

    fn pair() -> (CryptState, CryptState) {
        let a = [7u8; 16];
        let b = [9u8; 16];
        // client encrypts with `a`, server decrypts with `a`, and vice versa
        (CryptState::new(&KEY, &a, &b).unwrap(), CryptState::new(&KEY, &b, &a).unwrap())
    }

    #[test]
    fn ocb2_test_vectors() {
        // OCB2-AES128 vectors (key = nonce = 00..0f) as used by Mumble's TestCrypt.
        let cs = CryptState::new(&KEY, &KEY, &KEY).unwrap();
        let (c, tag) = cs.ocb_encrypt(&[], &KEY);
        assert!(c.is_empty());
        assert_eq!(tag, [0xBF, 0x31, 0x08, 0x13, 0x07, 0x73, 0xAD, 0x5E, 0xC7, 0x0E, 0xC6, 0x9E, 0x78, 0x75, 0xA7, 0xB0]);
        let plain: Vec<u8> = (0..40).collect();
        let (c, tag) = cs.ocb_encrypt(&plain, &KEY);
        assert_eq!(
            c,
            [
                0xF7, 0x5D, 0x6B, 0xC8, 0xB4, 0xDC, 0x8D, 0x66, 0xB8, 0x36, 0xA2, 0xB0, 0x8B, 0x32, 0xA6, 0x36, 0x9F, 0x1C, 0xD3,
                0xC5, 0x22, 0x8D, 0x79, 0xFD, 0x6C, 0x26, 0x7F, 0x5F, 0x6A, 0xA7, 0xB2, 0x31, 0xC7, 0xDF, 0xB9, 0xD5, 0x99, 0x51,
                0xAE, 0x9C
            ]
        );
        assert_eq!(tag, [0x9D, 0xB0, 0xCD, 0xF8, 0x80, 0xF7, 0x3E, 0x3E, 0x10, 0xD4, 0xEB, 0x32, 0x17, 0x76, 0x66, 0x88]);
    }

    #[test]
    fn roundtrip_all_lengths() {
        let (mut c, mut s) = pair();
        for len in 0..100usize {
            let msg: Vec<u8> = (0..len).map(|i| (i * 7 + len) as u8).collect();
            let enc = c.encrypt(&msg);
            assert_eq!(s.decrypt(&enc).as_deref(), Some(&msg[..]), "len {len}");
        }
    }

    #[test]
    fn tamper_replay_and_reorder() {
        let (mut c, mut s) = pair();
        let p: Vec<Vec<u8>> = (0..5u8).map(|i| c.encrypt(&[i; 20])).collect();
        let mut bad = p[0].clone();
        bad[10] ^= 1;
        assert!(s.decrypt(&bad).is_none());
        assert!(s.decrypt(&p[0]).is_some());
        assert!(s.decrypt(&p[0]).is_none(), "replay must fail");
        assert!(s.decrypt(&p[2]).is_some()); // p[1] lost
        assert_eq!(s.lost, 1);
        assert_eq!(s.decrypt(&p[1]).unwrap(), vec![1u8; 20]); // arrives late
        assert_eq!(s.late, 1);
        assert!(s.decrypt(&p[3]).is_some());
        // bursts of loss beyond Mumble's ±128 window recover without a resync
        for burst in [127usize, 200, 300, 1000] {
            for _ in 0..burst {
                c.encrypt(b"lost");
            }
            let enc = c.encrypt(b"after burst");
            assert_eq!(s.decrypt(&enc).as_deref(), Some(&b"after burst"[..]), "burst {burst}");
            let enc = c.encrypt(b"next");
            assert_eq!(s.decrypt(&enc).as_deref(), Some(&b"next"[..]));
        }
        // many packets incl. iv byte wraparound
        for i in 0..600u32 {
            let enc = c.encrypt(&i.to_le_bytes());
            assert_eq!(s.decrypt(&enc).unwrap(), i.to_le_bytes());
        }
    }
}
