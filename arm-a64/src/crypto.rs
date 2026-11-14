//! The ARMv8.0 Cryptographic Extension (AES, SHA-1, SHA-256, 64-bit PMULL). Optional: the
//! BCM2711's Cortex-A72s do not have it (`Cpu::crypto` off, ID_AA64ISAR0 says so); QEMU's
//! `cortex-a72` does, and `--cpu-crypto` turns it on here.
use crate::cpu::Cpu;

const SBOX: [u8; 256] = {
    // generated from the multiplicative inverse and the affine map (FIPS-197)
    let mut s = [0u8; 256];
    let mut p: u8 = 1;
    let mut q: u8 = 1;
    loop {
        // p * 3, q / 3 in GF(2^8)
        p = p ^ (p << 1) ^ if p & 0x80 != 0 { 0x1b } else { 0 };
        q ^= q << 1;
        q ^= q << 2;
        q ^= q << 4;
        if q & 0x80 != 0 { q ^= 0x09; }
        let x = q ^ q.rotate_left(1) ^ q.rotate_left(2) ^ q.rotate_left(3) ^ q.rotate_left(4);
        s[p as usize] = x ^ 0x63;
        if p == 1 { break; }
    }
    s[0] = 0x63;
    s
};

const INV_SBOX: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 { t[SBOX[i] as usize] = i as u8; i += 1; }
    t
};

fn xt(b: u8) -> u8 { (b << 1) ^ if b & 0x80 != 0 { 0x1b } else { 0 } }
fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut r = 0;
    while b != 0 { if b & 1 != 0 { r ^= a; } a = xt(a); b >>= 1; }
    r
}

fn bytes(v: u128) -> [u8; 16] { v.to_le_bytes() }
fn from(b: [u8; 16]) -> u128 { u128::from_le_bytes(b) }

/// AES state byte i is row i % 4, column i / 4.
fn shift_rows(s: [u8; 16], inv: bool) -> [u8; 16] {
    let mut o = [0; 16];
    for c in 0..4 {
        for r in 0..4 {
            let src = if inv { (c + 4 - r) % 4 } else { (c + r) % 4 };
            o[r + 4 * c] = s[r + 4 * src];
        }
    }
    o
}

fn mix_columns(s: [u8; 16], inv: bool) -> [u8; 16] {
    let m: [u8; 4] = if inv { [14, 11, 13, 9] } else { [2, 3, 1, 1] };
    let mut o = [0; 16];
    for c in 0..4 {
        for r in 0..4 {
            o[r + 4 * c] = (0..4).fold(0, |a, k| a ^ gmul(s[(r + k) % 4 + 4 * c], m[k]));
        }
    }
    o
}

fn w(v: u128, e: u32) -> u32 { (v >> (32 * e)) as u32 }
fn set_w(v: &mut u128, e: u32, x: u32) { *v = (*v & !(0xffff_ffffu128 << (32 * e))) | ((x as u128) << (32 * e)); }

fn sha256_hash(mut x: u128, mut y: u128, wv: u128, part1: bool) -> u128 {
    for e in 0..4 {
        let chs = ((w(y, 1) ^ w(y, 2)) & w(y, 0)) ^ w(y, 2);
        let maj = (w(x, 0) & w(x, 1)) | ((w(x, 0) | w(x, 1)) & w(x, 2));
        let s1 = w(y, 0).rotate_right(6) ^ w(y, 0).rotate_right(11) ^ w(y, 0).rotate_right(25);
        let s0 = w(x, 0).rotate_right(2) ^ w(x, 0).rotate_right(13) ^ w(x, 0).rotate_right(22);
        let t1 = w(y, 3).wrapping_add(s1).wrapping_add(chs).wrapping_add(w(wv, e));
        let x3 = w(x, 3); set_w(&mut x, 3, t1.wrapping_add(x3));
        set_w(&mut y, 3, t1.wrapping_add(s0).wrapping_add(maj));
        // <Y, X> = ROL(Y : X, 32)
        let (ytop, xtop) = (w(y, 3), w(x, 3));
        y = (y << 32) | xtop as u128;
        x = (x << 32) | ytop as u128;
    }
    if part1 { x } else { y }
}

fn clmul(a: u64, b: u64) -> u128 {
    let mut r = 0u128;
    for i in 0..64 { if b >> i & 1 != 0 { r ^= (a as u128) << i; } }
    r
}

impl Cpu {
    /// One Cryptographic Extension instruction, or None if `i` is not one (or it is disabled).
    pub(crate) fn crypto_op(&mut self, i: u32) -> Option<()> {
        if !self.crypto { return None; }
        let (rd, rn, rm) = ((i & 31) as usize, ((i >> 5) & 31) as usize, ((i >> 16) & 31) as usize);
        let (d, n, m) = (self.v[rd], self.v[rn], self.v[rm]);
        let r: u128 = match i & 0xffff_fc00 {
            0x4e28_4800 => from(shift_rows(bytes(d ^ n), false).map(|b| SBOX[b as usize])),      // AESE
            0x4e28_5800 => from(shift_rows(bytes(d ^ n), true).map(|b| INV_SBOX[b as usize])),   // AESD
            0x4e28_6800 => from(mix_columns(bytes(n), false)),                                  // AESMC
            0x4e28_7800 => from(mix_columns(bytes(n), true)),                                   // AESIMC
            0x5e28_0800 => (n as u32).rotate_left(30) as u128,                                  // SHA1H
            0x5e28_1800 => {                                                                    // SHA1SU1
                let t = d ^ (n >> 32);
                let mut r = 0;
                for e in 0..4 { set_w(&mut r, e, w(t, e).rotate_left(1)); }
                let r3 = w(r, 3); set_w(&mut r, 3, r3 ^ w(t, 0).rotate_left(2));
                r
            }
            0x5e28_2800 => {                                                                    // SHA256SU0
                let t = ((n as u32 as u128) << 96) | (d >> 32);
                let mut r = 0;
                for e in 0..4 { let x = w(t, e); set_w(&mut r, e, (x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3)).wrapping_add(w(d, e))); }
                r
            }
            _ => match i & 0xffe0_fc00 {
                0x5e00_0000 | 0x5e00_1000 | 0x5e00_2000 => {                                    // SHA1C, SHA1P, SHA1M
                    let (mut x, mut y) = (d, n as u32);
                    for e in 0..4 {
                        let (b, c, dd) = (w(x, 1), w(x, 2), w(x, 3));
                        let t = match (i >> 12) & 3 { 0 => ((c ^ dd) & b) ^ dd, 1 => b ^ c ^ dd, _ => (b & c) | ((b | c) & dd) };
                        y = y.wrapping_add(w(x, 0).rotate_left(5)).wrapping_add(t).wrapping_add(w(m, e));
                        let x1 = w(x, 1); set_w(&mut x, 1, x1.rotate_left(30));
                        let top = w(x, 3);
                        x = (x << 32) | y as u128;
                        y = top;
                    }
                    x
                }
                0x5e00_3000 => ((n as u64 as u128) << 64 | d >> 64) ^ d ^ m,                    // SHA1SU0
                0x5e00_4000 => sha256_hash(d, n, m, true),                                      // SHA256H
                0x5e00_5000 => sha256_hash(n, d, m, false),                                     // SHA256H2
                0x5e00_6000 => {                                                                // SHA256SU1
                    let t0 = ((m as u32 as u128) << 96) | (n >> 32);
                    let mut r = 0u128;
                    for e in 0..2 { let x = w(m, e + 2); set_w(&mut r, e, (x.rotate_right(17) ^ x.rotate_right(19) ^ (x >> 10)).wrapping_add(w(d, e)).wrapping_add(w(t0, e))); }
                    for e in 2..4 { let x = w(r, e - 2); set_w(&mut r, e, (x.rotate_right(17) ^ x.rotate_right(19) ^ (x >> 10)).wrapping_add(w(d, e)).wrapping_add(w(t0, e))); }
                    r
                }
                _ => match i & 0xbfe0_fc00 {
                    0x0ee0_e000 => {                                                            // PMULL{2} Vd.1Q, Vn.1D, Vm.1D
                        let hi = i >> 30 & 1 != 0;
                        let pick = |v: u128| if hi { (v >> 64) as u64 } else { v as u64 };
                        clmul(pick(n), pick(m))
                    }
                    _ => return None,
                },
            },
        };
        self.v[rd] = r;
        Some(())
    }
}
