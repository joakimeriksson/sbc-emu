//! The JH7110 security engine (`starfive,jh7110-sec`, 0x16000000): its public-key
//! accelerator (PKA), which StarFive's 5.15 kernel uses for RSA (`jh7110-pka.c`).
//!
//! The PKA is a Montgomery engine over three operand registers: A (CAAR), E (CAER) and the
//! modulus N (CANR), each 64 32-bit words. Operands are `opsize + 1` words long, least
//! significant word first, each word holding four big-endian bytes. With R = 2^(32·words):
//! - PRE prepares the modulus (R² mod N, kept internally);
//! - ARN: A = E·R mod N (into the Montgomery domain);
//! - AARN: A = A·A·R⁻¹ mod N (square);
//! - AERN: A = A·E·R⁻¹ mod N (multiply; with E = 1 it leaves the domain).
//!
//! A command with `ie` raises the engine's interrupt when done (at once here); writing
//! CACR with `cln_done` clears the done flag. The AES and SHA engines are not modelled.

const CACR: u64 = 0x400;
const CASR: u64 = 0x404;
const CAAR: u64 = 0x408;
const CAER: u64 = 0x508;
const CANR: u64 = 0x608;
const WORDS: usize = 64;

const CMD_PRE: u32 = 0;
const CMD_ARN: u32 = 5;
const CMD_AERN: u32 = 6;
const CMD_AARN: u32 = 7;

pub struct Sec {
    regs: Vec<u32>,
    a: [u32; WORDS],
    e: [u32; WORDS],
    n: [u32; WORDS],
    cacr: u32,
    done: bool,
    /// operations executed, for reports
    pub ops: u64,
}

impl Default for Sec {
    fn default() -> Self {
        Sec { regs: vec![0; 0x1000], a: [0; WORDS], e: [0; WORDS], n: [0; WORDS], cacr: 0, done: false, ops: 0 }
    }
}

/// A register word as a number limb (the hardware stores big-endian bytes).
fn limb(w: u32) -> u32 {
    w.swap_bytes()
}

impl Sec {
    pub fn irq(&self) -> bool {
        self.done && self.cacr & 4 != 0
    }

    pub fn read(&self, off: u64) -> u32 {
        match off {
            CACR => self.cacr,
            CASR => self.done as u32,
            _ if (CAAR..CAAR + 4 * WORDS as u64).contains(&off) => self.a[((off - CAAR) / 4) as usize],
            _ if (CAER..CAER + 4 * WORDS as u64).contains(&off) => self.e[((off - CAER) / 4) as usize],
            _ if (CANR..CANR + 4 * WORDS as u64).contains(&off) => self.n[((off - CANR) / 4) as usize],
            _ => self.regs.get((off / 4) as usize).copied().unwrap_or(0),
        }
    }

    pub fn write(&mut self, off: u64, v: u32) {
        match off {
            CACR => {
                self.cacr = v & !1;
                if v & (1 << 15) != 0 {
                    self.done = false;
                }
                if v & 2 != 0 {
                    self.done = false;
                }
                if v & 1 != 0 {
                    let words = (((v >> 16) & 0x3f) + 1) as usize;
                    self.execute((v >> 8) & 0xf, words);
                    self.done = true;
                    self.ops = self.ops.wrapping_add(1);
                }
            }
            CASR => {}
            _ if (CAAR..CAAR + 4 * WORDS as u64).contains(&off) => self.a[((off - CAAR) / 4) as usize] = v,
            _ if (CAER..CAER + 4 * WORDS as u64).contains(&off) => self.e[((off - CAER) / 4) as usize] = v,
            _ if (CANR..CANR + 4 * WORDS as u64).contains(&off) => self.n[((off - CANR) / 4) as usize] = v,
            _ => {
                if let Some(r) = self.regs.get_mut((off / 4) as usize) {
                    *r = v;
                }
            }
        }
    }

    fn get(reg: &[u32; WORDS], words: usize) -> Vec<u32> {
        reg[..words].iter().map(|&w| limb(w)).collect()
    }

    fn put(reg: &mut [u32; WORDS], value: &[u32]) {
        for (k, &l) in value.iter().enumerate() {
            reg[k] = limb(l);
        }
    }

    fn execute(&mut self, cmd: u32, words: usize) {
        let n = Self::get(&self.n, words);
        if n[0] & 1 == 0 {
            return; // Montgomery arithmetic needs an odd modulus
        }
        let result = match cmd {
            CMD_PRE => return,
            CMD_ARN => {
                let r2 = r_squared(&n);
                mont_mul(&Self::get(&self.e, words), &r2, &n)
            }
            CMD_AARN => {
                let a = Self::get(&self.a, words);
                mont_mul(&a, &a, &n)
            }
            CMD_AERN => mont_mul(&Self::get(&self.a, words), &Self::get(&self.e, words), &n),
            _ => return,
        };
        Self::put(&mut self.a, &result);
    }
}

/// a >= b for equal-length little-endian limb vectors
fn ge(a: &[u32], b: &[u32]) -> bool {
    for k in (0..a.len()).rev() {
        if a[k] != b[k] {
            return a[k] > b[k];
        }
    }
    true
}

fn sub_in_place(a: &mut [u32], b: &[u32]) {
    let mut borrow = 0i64;
    for k in 0..a.len() {
        let d = a[k] as i64 - b[k] as i64 - borrow;
        a[k] = d as u32;
        borrow = (d < 0) as i64;
    }
}

/// R² mod N with R = 2^(32·len): 1 doubled 2·32·len times, reduced each time.
fn r_squared(n: &[u32]) -> Vec<u32> {
    let len = n.len();
    let mut x = vec![0u32; len];
    x[0] = 1;
    for _ in 0..64 * len {
        let mut carry = 0u32;
        for w in x.iter_mut() {
            let nc = *w >> 31;
            *w = (*w << 1) | carry;
            carry = nc;
        }
        if carry != 0 || ge(&x, n) {
            sub_in_place(&mut x, n);
        }
    }
    x
}

/// Montgomery multiplication (CIOS): a·b·R⁻¹ mod N, for a, b < N and N odd.
fn mont_mul(a: &[u32], b: &[u32], n: &[u32]) -> Vec<u32> {
    let len = n.len();
    // n' = -N⁻¹ mod 2^32 (Newton iteration)
    let mut inv: u32 = 1;
    for _ in 0..5 {
        inv = inv.wrapping_mul(2u32.wrapping_sub(n[0].wrapping_mul(inv)));
    }
    let n_prime = inv.wrapping_neg();
    let mut t = vec![0u64; len + 2];
    for &ai in a.iter().take(len) {
        let mut carry = 0u64;
        for j in 0..len {
            let s = t[j] + ai as u64 * b[j] as u64 + carry;
            t[j] = s & 0xffff_ffff;
            carry = s >> 32;
        }
        let s = t[len] + carry;
        t[len] = s & 0xffff_ffff;
        t[len + 1] += s >> 32;
        let m = (t[0] as u32).wrapping_mul(n_prime) as u64;
        let mut carry = (t[0] + m * n[0] as u64) >> 32;
        for j in 1..len {
            let s = t[j] + m * n[j] as u64 + carry;
            t[j - 1] = s & 0xffff_ffff;
            carry = s >> 32;
        }
        let s = t[len] + carry;
        t[len - 1] = s & 0xffff_ffff;
        t[len] = t[len + 1] + (s >> 32);
        t[len + 1] = 0;
    }
    let mut out: Vec<u32> = t[..len].iter().map(|&w| w as u32).collect();
    if t[len] != 0 || ge(&out, n) {
        sub_in_place(&mut out, n);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(sec: &mut Sec, base: u64, v: &[u32]) {
        for (k, &l) in v.iter().enumerate() {
            sec.write(base + 4 * k as u64, l.swap_bytes());
        }
    }

    fn cmd(sec: &mut Sec, c: u32, words: u32) {
        sec.write(CACR, 1 | 4 | (c << 8) | (1 << 15) | ((words - 1) << 16));
        assert!(sec.irq());
        sec.write(CACR, 1 << 15);
        assert!(!sec.irq());
    }

    /// The driver's RSA sequence: 7^13 mod 1000003 (two words) with the engine.
    #[test]
    fn rsa_powm_as_the_driver_runs_it() {
        let n: u64 = 0xffff_fffb_0000_0001 % 0xffff_ffff_ffff_ffc5 | 1; // an odd 64-bit modulus
        let (base, exp) = (0x1234_5678_9abc_def0u64 % n, 65537u64);
        let lo = |v: u64| [v as u32, (v >> 32) as u32];
        let mut sec = Sec::default();
        set(&mut sec, CANR, &lo(n));
        // into the Montgomery domain: E = base; ARN
        cmd(&mut sec, CMD_PRE, 2);
        set(&mut sec, CAER, &lo(base));
        cmd(&mut sec, CMD_ARN, 2);
        let mta: Vec<u32> = (0..2).map(|k| sec.read(CAAR + 4 * k)).collect();
        for (k, &w) in mta.iter().enumerate() {
            sec.write(CAER + 4 * k as u64, w);
        }
        // square-and-multiply from the top bit
        for bit in (0..63 - exp.leading_zeros()).rev() {
            cmd(&mut sec, CMD_AARN, 2);
            if exp >> bit & 1 != 0 {
                cmd(&mut sec, CMD_AERN, 2);
            }
        }
        // out of the domain: E = 1; AERN
        set(&mut sec, CAER, &[1, 0]);
        cmd(&mut sec, CMD_AERN, 2);
        let got = (sec.read(CAAR).swap_bytes() as u64) | (sec.read(CAAR + 4).swap_bytes() as u64) << 32;
        let mut want = 1u128;
        for _ in 0..exp {
            want = want * base as u128 % n as u128;
        }
        assert_eq!(got as u128, want);
    }
}

emu_core::snap_struct!(Sec { a, e, n, cacr, done, ops } fixed { regs });
