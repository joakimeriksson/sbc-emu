//! The macro method expander: small programs the driver uploads, called by writing a method from
//! 0x3800 up, which read parameters and write methods. 32-bit instructions over eight registers
//! ($r0 reads as zero).

const WORDS: usize = 0x2000;

pub struct Mme {
    code: Vec<u32>,
    code_at: usize,
    start: [u32; 256],
    start_at: usize,
    /// the macro being called and its parameters so far
    call: Option<(usize, Vec<u32>)>,
}

impl Default for Mme {
    fn default() -> Self {
        Self { code: vec![0; WORDS], code_at: 0, start: [0; 256], start_at: 0, call: None }
    }
}

/// The method state a macro reads, and the methods it writes.
pub trait Methods {
    fn read(&mut self, method: u32) -> u32;
    fn write(&mut self, method: u32, v: u32);
}

impl Mme {
    /// The upload methods: true if it was one.
    pub fn upload(&mut self, method: u32, v: u32) -> bool {
        match method {
            0x114 => self.code_at = v as usize % WORDS,
            0x118 => {
                self.code[self.code_at] = v;
                self.code_at = (self.code_at + 1) % WORDS;
            }
            0x11c => self.start_at = v as usize % 256,
            0x120 => {
                self.start[self.start_at] = v;
                self.start_at = (self.start_at + 1) % 256;
            }
            _ => return false,
        }
        true
    }

    /// A call method (0x3800 + 8 k: the first parameter, + 4: the others). The macro runs when its
    /// group of methods ends (`take_call`).
    pub fn call(&mut self, method: u32, v: u32) -> Option<(usize, Vec<u32>)> {
        let k = (method as usize - 0x3800) / 8;
        if method & 4 == 0 {
            return self.call.replace((k, vec![v]));
        }
        if let Some((_, p)) = self.call.as_mut() {
            p.push(v);
        }
        None
    }

    pub fn take_call(&mut self) -> Option<(usize, Vec<u32>)> {
        self.call.take()
    }

    pub fn calling(&self) -> bool {
        self.call.is_some()
    }

    /// The code and entry point of macro `k`.
    pub fn program(&self, k: usize) -> (&[u32], usize) {
        (&self.code, self.start.get(k).copied().unwrap_or(0) as usize)
    }
}

/// Run a macro.
pub fn run(code: &[u32], entry: usize, params: &[u32], m: &mut dyn Methods) {
    let mut r = [0u32; 8];
    r[1] = params.first().copied().unwrap_or(0);
    let mut next = params.iter().skip(1).copied();
    let (mut pc, mut address, mut step, mut carry) = (entry, 0u32, 0u32, 0u32);
    // (the pc after the delay slot), whether the next instruction is the last
    let (mut branch, mut last) = (None::<usize>, false);
    for _ in 0..1_000_000 {
        let Some(&i) = code.get(pc) else { return };
        let (dst, a, b) = ((i >> 8 & 7) as usize, (i >> 11 & 7) as usize, (i >> 14 & 7) as usize);
        let imm = (i as i32 >> 14) as u32;
        let ending = last;
        let taken = branch.take();
        let exit = i >> 7 & 1 != 0 && i & 7 != 7;
        let mut skip_delay = false;
        if i & 7 == 7 {
            // branch on a register being zero (or not); bit 5: no delay slot
            if (r[a] != 0) == (i >> 4 & 1 != 0) {
                let target = (pc as i64 + imm as i32 as i64) as usize;
                if i >> 5 & 1 != 0 {
                    skip_delay = true;
                    branch = Some(target);
                } else {
                    branch = Some(target);
                }
            }
        } else {
            let (size, from, to) = (i >> 22 & 31, i >> 17 & 31, i >> 27);
            let mask = (1u64 << size) as u32;
            let mask = mask.wrapping_sub(1);
            let v = match i & 7 {
                0 => alu(i >> 17 & 31, r[a], r[b], &mut carry),
                1 => r[a].wrapping_add(imm),
                2 => r[a] & !(mask << to) | (r[b] >> from & mask) << to,
                3 => (r[b].checked_shr(r[a]).unwrap_or(0) & mask) << to,
                4 => (r[b] >> from & mask).checked_shl(r[a]).unwrap_or(0),
                5 => m.read((r[a].wrapping_add(imm) & 0xfff) << 2),
                _ => 0,
            };
            let set = |reg: &mut [u32; 8], v: u32| if dst != 0 { reg[dst] = v };
            let mut send = |v: u32, address: &mut u32| {
                m.write((*address & 0xfff) << 2, v);
                *address = address.wrapping_add(step);
            };
            match i >> 4 & 7 {
                0 => set(&mut r, next.next().unwrap_or(0)),
                1 => set(&mut r, v),
                2 => {
                    set(&mut r, v);
                    (address, step) = (v & 0xfff, v >> 12 & 0x3f);
                }
                3 => {
                    set(&mut r, next.next().unwrap_or(0));
                    send(v, &mut address);
                }
                4 => {
                    set(&mut r, v);
                    send(v, &mut address);
                }
                5 => {
                    set(&mut r, next.next().unwrap_or(0));
                    (address, step) = (v & 0xfff, v >> 12 & 0x3f);
                }
                6 => {
                    set(&mut r, v);
                    (address, step) = (v & 0xfff, v >> 12 & 0x3f);
                    let p = next.next().unwrap_or(0);
                    m.write((address & 0xfff) << 2, p);
                    address = address.wrapping_add(step);
                }
                _ => {
                    set(&mut r, v);
                    (address, step) = (v & 0xfff, v >> 12 & 0x3f);
                    m.write((address & 0xfff) << 2, v >> 12 & 0x3f);
                    address = address.wrapping_add(step);
                }
            }
        }
        if ending {
            return;
        }
        last = exit;
        pc = match taken {
            Some(t) => t,
            None if skip_delay => branch.take().unwrap(),
            None => pc + 1,
        };
    }
}

fn alu(op: u32, a: u32, b: u32, carry: &mut u32) -> u32 {
    let (v, c) = match op {
        0 => a.overflowing_add(b),
        1 => {
            let s = a as u64 + b as u64 + *carry as u64;
            (s as u32, s >> 32 != 0)
        }
        2 => (a.wrapping_sub(b), a >= b),
        3 => {
            let s = (a as u64).wrapping_sub(b as u64).wrapping_sub(1 - *carry as u64);
            (s as u32, s >> 63 == 0)
        }
        8 => return a ^ b,
        9 => return a | b,
        10 => return a & b,
        11 => return a & !b,
        12 => return !(a & b),
        _ => return 0,
    };
    *carry = c as u32;
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    struct M(Vec<(u32, u32)>);
    impl Methods for M {
        fn read(&mut self, method: u32) -> u32 { method * 10 }
        fn write(&mut self, method: u32, v: u32) { self.0.push((method, v)) }
    }

    fn addi(dst: u32, a: u32, imm: i32, res: u32) -> u32 {
        (imm as u32) << 14 | a << 11 | dst << 8 | res << 4 | 1
    }

    #[test]
    fn methods_loops_and_delay_slots() {
        // $r2 = next parameter (count); method address 0x600 step 1; send $r1 (+1 each) count times
        let code = [
            addi(2, 0, 0, 0),                               // $r2 = parameter
            addi(0, 0, 0x600 | 1 << 12, 2),                 // address
            addi(1, 1, 1, 1) & !0x70 | 4 << 4,              // $r1 += 1, send
            addi(2, 2, -1, 1),                              // $r2 -= 1
            (-2i32 as u32) << 14 | 2 << 11 | 1 << 4 | 7,    // until zero
            addi(0, 0, 0, 1),                               // delay slot
            addi(0, 0, 0, 1) | 1 << 7,                      // exit
            addi(0, 0, 77, 4),                              // its delay slot: send 77
            addi(0, 0, 99, 4),                              // not reached
        ];
        let mut m = M(Vec::new());
        run(&code, 0, &[5, 3], &mut m);
        assert_eq!(m.0, [(0x1800, 6), (0x1804, 7), (0x1808, 8), (0x180c, 77)]);
    }

    #[test]
    fn bit_fields_and_state() {
        let read = addi(3, 0, 0x40, 1) & !7 | 5; // $r3 = state of method 0x100
        // insert bits 4..12 of $r1 at bit 16 of $r3 (op 2: a = base, b = source)
        let insert = 16 << 27 | 8 << 22 | 4 << 17 | 1 << 14 | 3 << 11 | 4 << 8 | 1 << 4 | 2;
        let send = addi(0, 4, 0, 4);
        let code = [read, insert, addi(0, 0, 0x10, 2) | 1 << 7, send];
        let mut m = M(Vec::new());
        run(&code, 0, &[0xabc], &mut m);
        assert_eq!(m.0, [(0x40, 0xab << 16 | 2560)]);
    }

    #[test]
    fn calls_collect_parameters() {
        let mut mme = Mme::default();
        assert!(mme.upload(0x114, 5) && mme.upload(0x118, 7) && mme.upload(0x11c, 2) && mme.upload(0x120, 5));
        assert_eq!(mme.program(2), (&mme.code[..], 5));
        assert_eq!(mme.call(0x3810, 1), None);
        mme.call(0x3814, 2);
        assert_eq!(mme.call(0x3818, 9), Some((2, vec![1, 2])));
        assert_eq!(mme.take_call(), Some((3, vec![9])));
    }
}
