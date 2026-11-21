//! Instruction semantics: RV64I, M, A, Zicsr, Zifencei, Zba, Zbb and the privileged
//! instructions; F and D are in `fpu.rs`.
use crate::{exc, mstatus, prv, Access, Bus, Exception, Hart, R};

#[inline(always)]
fn rd(i: u32) -> usize {
    ((i >> 7) & 31) as usize
}
#[inline(always)]
fn rs1(i: u32) -> usize {
    ((i >> 15) & 31) as usize
}
#[inline(always)]
fn rs2(i: u32) -> usize {
    ((i >> 20) & 31) as usize
}
#[inline(always)]
fn f3(i: u32) -> u32 {
    (i >> 12) & 7
}
#[inline(always)]
fn f7(i: u32) -> u32 {
    i >> 25
}
#[inline(always)]
fn imm_i(i: u32) -> u64 {
    ((i as i32) >> 20) as i64 as u64
}
#[inline(always)]
fn imm_s(i: u32) -> u64 {
    ((((i as i32) >> 25) << 5) | ((i >> 7) & 31) as i32) as i64 as u64
}
#[inline(always)]
fn imm_b(i: u32) -> u64 {
    let v = (((i as i32) >> 31) << 12) as u32 | ((i >> 7) & 1) << 11 | ((i >> 25) & 0x3f) << 5 | ((i >> 8) & 0xf) << 1;
    v as i32 as i64 as u64
}
#[inline(always)]
fn imm_j(i: u32) -> u64 {
    let v = (((i as i32) >> 31) << 20) as u32 | (i & 0xff000) | ((i >> 20) & 1) << 11 | ((i >> 21) & 0x3ff) << 1;
    v as i32 as i64 as u64
}
#[inline(always)]
fn sext32(v: u64) -> u64 {
    v as i32 as i64 as u64
}

impl Hart {
    #[inline(always)]
    pub(crate) fn set(&mut self, r: usize, v: u64) {
        if r != 0 {
            self.x[r] = v;
        }
    }

    // ---- memory ----

    #[inline(always)]
    pub(crate) fn load<B: Bus>(&mut self, bus: &mut B, va: u64, size: u8) -> R<u64> {
        if (va & 0xfff) + size as u64 > 0x1000 {
            return self.load_split(bus, va, size);
        }
        let pa = self.translate(bus, va, Access::Load).map_err(|cause| Exception { cause, tval: va })?;
        let base = bus.ram_base();
        let ram = bus.ram();
        let o = pa.wrapping_sub(base);
        if o + (size as u64) <= ram.len() as u64 {
            let o = o as usize;
            return Ok(match size {
                1 => ram[o] as u64,
                2 => u16::from_le_bytes(ram[o..o + 2].try_into().unwrap()) as u64,
                4 => u32::from_le_bytes(ram[o..o + 4].try_into().unwrap()) as u64,
                _ => u64::from_le_bytes(ram[o..o + 8].try_into().unwrap()),
            });
        }
        self.dev_access = true;
        bus.read(pa, size).map_err(|_| Exception { cause: exc::LOAD_ACCESS, tval: va })
    }

    #[cold]
    fn load_split<B: Bus>(&mut self, bus: &mut B, va: u64, size: u8) -> R<u64> {
        let mut v = 0;
        for k in 0..size as u64 {
            v |= self.load(bus, va + k, 1)? << (8 * k);
        }
        Ok(v)
    }

    #[inline(always)]
    pub(crate) fn store<B: Bus>(&mut self, bus: &mut B, va: u64, size: u8, v: u64) -> R<()> {
        if (va & 0xfff) + size as u64 > 0x1000 {
            return self.store_split(bus, va, size, v);
        }
        let pa = self.translate(bus, va, Access::Store).map_err(|cause| Exception { cause, tval: va })?;
        if self.reservation != u64::MAX && pa & !7 == self.reservation & !7 {
            self.reservation = u64::MAX;
        }
        let base = bus.ram_base();
        let ram = bus.ram();
        let o = pa.wrapping_sub(base);
        if o + (size as u64) <= ram.len() as u64 {
            let o = o as usize;
            match size {
                1 => ram[o] = v as u8,
                2 => ram[o..o + 2].copy_from_slice(&(v as u16).to_le_bytes()),
                4 => ram[o..o + 4].copy_from_slice(&(v as u32).to_le_bytes()),
                _ => ram[o..o + 8].copy_from_slice(&v.to_le_bytes()),
            }
            return Ok(());
        }
        self.dev_access = true;
        bus.write(pa, size, v).map_err(|_| Exception { cause: exc::STORE_ACCESS, tval: va })
    }

    #[cold]
    fn store_split<B: Bus>(&mut self, bus: &mut B, va: u64, size: u8, v: u64) -> R<()> {
        // check both pages first, so a fault leaves memory unchanged
        let last = va + size as u64 - 1;
        self.translate(bus, va, Access::Store).map_err(|cause| Exception { cause, tval: va })?;
        self.translate(bus, last, Access::Store).map_err(|cause| Exception { cause, tval: last })?;
        for k in 0..size as u64 {
            self.store(bus, va + k, 1, v >> (8 * k))?;
        }
        Ok(())
    }

    /// An AMO's or LR/SC's physical address (aligned; faults as a store).
    fn amo_addr<B: Bus>(&mut self, bus: &mut B, va: u64, size: u8, access: Access) -> R<u64> {
        if va & (size as u64 - 1) != 0 {
            let cause = if access == Access::Load { exc::LOAD_MISALIGNED } else { exc::STORE_MISALIGNED };
            return Err(Exception { cause, tval: va });
        }
        self.translate(bus, va, access).map_err(|cause| Exception { cause, tval: va })
    }

    fn phys_read<B: Bus>(bus: &mut B, pa: u64, size: u8, va: u64, cause: u64) -> R<u64> {
        let base = bus.ram_base();
        let ram = bus.ram();
        let o = pa.wrapping_sub(base);
        if o + (size as u64) <= ram.len() as u64 {
            let o = o as usize;
            return Ok(if size == 4 {
                u32::from_le_bytes(ram[o..o + 4].try_into().unwrap()) as u64
            } else {
                u64::from_le_bytes(ram[o..o + 8].try_into().unwrap())
            });
        }
        bus.read(pa, size).map_err(|_| Exception { cause, tval: va })
    }

    fn phys_write<B: Bus>(bus: &mut B, pa: u64, size: u8, v: u64, va: u64) -> R<()> {
        let base = bus.ram_base();
        let ram = bus.ram();
        let o = pa.wrapping_sub(base);
        if o + (size as u64) <= ram.len() as u64 {
            let o = o as usize;
            if size == 4 {
                ram[o..o + 4].copy_from_slice(&(v as u32).to_le_bytes());
            } else {
                ram[o..o + 8].copy_from_slice(&v.to_le_bytes());
            }
            return Ok(());
        }
        bus.write(pa, size, v).map_err(|_| Exception { cause: exc::STORE_ACCESS, tval: va })
    }

    // ---- execution ----

    /// Execute one 32-bit instruction (`len` = 2 for an expanded compressed one).
    pub(crate) fn exec<B: Bus>(&mut self, bus: &mut B, i: u32, len: u64) -> R<()> {
        let pc = self.pc;
        let next = pc.wrapping_add(len);
        let x1 = self.x[rs1(i)];
        let x2 = self.x[rs2(i)];
        let illegal = Exception::illegal(if len == 2 { 0 } else { i });
        match i & 0x7f {
            0x37 => self.set(rd(i), (i & 0xffff_f000) as i32 as i64 as u64),
            0x17 => self.set(rd(i), pc.wrapping_add((i & 0xffff_f000) as i32 as i64 as u64)),
            0x6f => {
                let t = pc.wrapping_add(imm_j(i));
                self.set(rd(i), next);
                self.pc = t;
                return Ok(());
            }
            0x67 => {
                if f3(i) != 0 {
                    return Err(illegal);
                }
                let t = x1.wrapping_add(imm_i(i)) & !1;
                self.set(rd(i), next);
                self.pc = t;
                return Ok(());
            }
            0x63 => {
                let taken = match f3(i) {
                    0 => x1 == x2,
                    1 => x1 != x2,
                    4 => (x1 as i64) < (x2 as i64),
                    5 => (x1 as i64) >= (x2 as i64),
                    6 => x1 < x2,
                    7 => x1 >= x2,
                    _ => return Err(illegal),
                };
                if taken {
                    self.pc = pc.wrapping_add(imm_b(i));
                    return Ok(());
                }
            }
            0x03 => {
                let a = x1.wrapping_add(imm_i(i));
                let v = match f3(i) {
                    0 => self.load(bus, a, 1)? as i8 as i64 as u64,
                    1 => self.load(bus, a, 2)? as i16 as i64 as u64,
                    2 => self.load(bus, a, 4)? as i32 as i64 as u64,
                    3 => self.load(bus, a, 8)?,
                    4 => self.load(bus, a, 1)?,
                    5 => self.load(bus, a, 2)?,
                    6 => self.load(bus, a, 4)?,
                    _ => return Err(illegal),
                };
                self.set(rd(i), v);
            }
            0x23 => {
                let a = x1.wrapping_add(imm_s(i));
                match f3(i) {
                    0 => self.store(bus, a, 1, x2)?,
                    1 => self.store(bus, a, 2, x2)?,
                    2 => self.store(bus, a, 4, x2)?,
                    3 => self.store(bus, a, 8, x2)?,
                    _ => return Err(illegal),
                }
            }
            0x13 => {
                let imm = imm_i(i);
                let sh = (i >> 20) & 63;
                let v = match f3(i) {
                    0 => x1.wrapping_add(imm),
                    2 => ((x1 as i64) < (imm as i64)) as u64,
                    3 => (x1 < imm) as u64,
                    4 => x1 ^ imm,
                    6 => x1 | imm,
                    7 => x1 & imm,
                    1 => match i >> 26 {
                        0 => x1 << sh,
                        0x18 => match (i >> 20) & 0x3f {
                            // Zbb unary ops (funct12 0x600..0x605)
                            0 => x1.leading_zeros() as u64,
                            1 => x1.trailing_zeros() as u64,
                            2 => x1.count_ones() as u64,
                            4 => x1 as i8 as i64 as u64,
                            5 => x1 as i16 as i64 as u64,
                            _ => return Err(illegal),
                        },
                        _ => return Err(illegal),
                    },
                    _ => match i >> 26 {
                        0 => x1 >> sh,
                        0x10 => ((x1 as i64) >> sh) as u64,
                        0x18 => x1.rotate_right(sh),
                        _ => match i >> 20 {
                            0x287 => {
                                // orc.b
                                let mut r = 0u64;
                                for b in 0..8 {
                                    if (x1 >> (8 * b)) & 0xff != 0 {
                                        r |= 0xff << (8 * b);
                                    }
                                }
                                r
                            }
                            0x6b8 => x1.swap_bytes(),
                            _ => return Err(illegal),
                        },
                    },
                };
                self.set(rd(i), v);
            }
            0x1b => {
                let sh = (i >> 20) & 31;
                let v = match (f3(i), f7(i)) {
                    (0, _) => sext32(x1.wrapping_add(imm_i(i))),
                    (1, 0) => sext32(x1 << sh),
                    (1, 4) | (1, 5) => (x1 & 0xffff_ffff) << ((i >> 20) & 63), // slli.uw
                    (1, 0x30) => match rs2(i) {
                        0 => (x1 as u32).leading_zeros() as u64,
                        1 => (x1 as u32).trailing_zeros() as u64,
                        2 => (x1 as u32).count_ones() as u64,
                        _ => return Err(illegal),
                    },
                    (5, 0) => sext32(((x1 as u32) >> sh) as u64),
                    (5, 0x20) => ((x1 as i32) >> sh) as i64 as u64,
                    (5, 0x30) => sext32((x1 as u32).rotate_right(sh) as u64),
                    _ => return Err(illegal),
                };
                self.set(rd(i), v);
            }
            0x33 => {
                let v = match (f7(i), f3(i)) {
                    (0, 0) => x1.wrapping_add(x2),
                    (0x20, 0) => x1.wrapping_sub(x2),
                    (0, 1) => x1 << (x2 & 63),
                    (0, 2) => ((x1 as i64) < (x2 as i64)) as u64,
                    (0, 3) => (x1 < x2) as u64,
                    (0, 4) => x1 ^ x2,
                    (0, 5) => x1 >> (x2 & 63),
                    (0x20, 5) => ((x1 as i64) >> (x2 & 63)) as u64,
                    (0, 6) => x1 | x2,
                    (0, 7) => x1 & x2,
                    (1, f) => mul_div(f, x1, x2),
                    // Zbb
                    (0x20, 7) => x1 & !x2,
                    (0x20, 6) => x1 | !x2,
                    (0x20, 4) => !(x1 ^ x2),
                    (0x05, 4) => (x1 as i64).min(x2 as i64) as u64,
                    (0x05, 5) => x1.min(x2),
                    (0x05, 6) => (x1 as i64).max(x2 as i64) as u64,
                    (0x05, 7) => x1.max(x2),
                    (0x30, 1) => x1.rotate_left((x2 & 63) as u32),
                    (0x30, 5) => x1.rotate_right((x2 & 63) as u32),
                    // Zba
                    (0x10, 2) => (x1 << 1).wrapping_add(x2),
                    (0x10, 4) => (x1 << 2).wrapping_add(x2),
                    (0x10, 6) => (x1 << 3).wrapping_add(x2),
                    _ => return Err(illegal),
                };
                self.set(rd(i), v);
            }
            0x3b => {
                let (a, b) = (x1 as u32, x2 as u32);
                let v = match (f7(i), f3(i)) {
                    (0, 0) => sext32(a.wrapping_add(b) as u64),
                    (0x20, 0) => sext32(a.wrapping_sub(b) as u64),
                    (0, 1) => sext32((a << (b & 31)) as u64),
                    (0, 5) => sext32((a >> (b & 31)) as u64),
                    (0x20, 5) => ((a as i32) >> (b & 31)) as i64 as u64,
                    (1, f) => mul_div_w(f, a, b),
                    // Zba
                    (0x04, 0) => (x1 & 0xffff_ffff).wrapping_add(x2),
                    (0x10, 2) => ((x1 & 0xffff_ffff) << 1).wrapping_add(x2),
                    (0x10, 4) => ((x1 & 0xffff_ffff) << 2).wrapping_add(x2),
                    (0x10, 6) => ((x1 & 0xffff_ffff) << 3).wrapping_add(x2),
                    // Zbb
                    (0x04, 4) if rs2(i) == 0 => x1 & 0xffff,
                    (0x30, 1) => sext32(a.rotate_left(b & 31) as u64),
                    (0x30, 5) => sext32(a.rotate_right(b & 31) as u64),
                    _ => return Err(illegal),
                };
                self.set(rd(i), v);
            }
            0x0f => {
                // fence, fence.i: memory is sequentially consistent; instructions are
                // decoded from memory on every fetch
                if f3(i) == 1 {
                    self.fence_i += 1;
                }
            }
            0x2f => self.amo(bus, i, x1, x2)?,
            0x73 => return self.system(bus, i, len),
            0x07 | 0x27 | 0x43 | 0x47 | 0x4b | 0x4f | 0x53 => self.fp(bus, i, len)?,
            _ => return Err(illegal),
        }
        self.pc = next;
        Ok(())
    }

    fn amo<B: Bus>(&mut self, bus: &mut B, i: u32, x1: u64, x2: u64) -> R<()> {
        let size: u8 = match f3(i) {
            2 => 4,
            3 => 8,
            _ => return Err(Exception::illegal(i)),
        };
        let op = i >> 27;
        let ext = |v: u64| if size == 4 { sext32(v) } else { v };
        match op {
            0x02 => {
                // lr
                if rs2(i) != 0 {
                    return Err(Exception::illegal(i));
                }
                let pa = self.amo_addr(bus, x1, size, Access::Load)?;
                let v = Self::phys_read(bus, pa, size, x1, exc::LOAD_ACCESS)?;
                self.reservation = pa;
                self.set(rd(i), ext(v));
            }
            0x03 => {
                // sc
                let pa = self.amo_addr(bus, x1, size, Access::Store)?;
                if self.reservation == pa {
                    Self::phys_write(bus, pa, size, x2, x1)?;
                    self.set(rd(i), 0);
                } else {
                    self.set(rd(i), 1);
                }
                self.reservation = u64::MAX;
            }
            _ => {
                let pa = self.amo_addr(bus, x1, size, Access::Store)?;
                let old = Self::phys_read(bus, pa, size, x1, exc::STORE_ACCESS)?;
                let (a, b) = if size == 4 { (sext32(old), sext32(x2)) } else { (old, x2) };
                let new = match op {
                    0x01 => b,
                    0x00 => a.wrapping_add(b),
                    0x04 => a ^ b,
                    0x0c => a & b,
                    0x08 => a | b,
                    0x10 => (a as i64).min(b as i64) as u64,
                    0x14 => (a as i64).max(b as i64) as u64,
                    0x18 => if size == 4 { (old as u32).min(x2 as u32) as u64 } else { a.min(b) },
                    0x1c => if size == 4 { (old as u32).max(x2 as u32) as u64 } else { a.max(b) },
                    _ => return Err(Exception::illegal(i)),
                };
                Self::phys_write(bus, pa, size, new, x1)?;
                if self.reservation & !7 == pa & !7 {
                    self.reservation = u64::MAX;
                }
                self.set(rd(i), ext(old));
            }
        }
        Ok(())
    }

    fn system<B: Bus>(&mut self, bus: &mut B, i: u32, len: u64) -> R<()> {
        let illegal = Exception::illegal(i);
        let next = self.pc.wrapping_add(len);
        let f = f3(i);
        if f == 0 {
            if rd(i) != 0 {
                return Err(illegal);
            }
            match i >> 20 {
                0x000 if rs1(i) == 0 => {
                    let cause = match self.prv {
                        prv::U => exc::ECALL_U,
                        prv::S => exc::ECALL_S,
                        _ => exc::ECALL_M,
                    };
                    return Err(Exception { cause, tval: 0 });
                }
                0x001 if rs1(i) == 0 => return Err(Exception { cause: exc::BREAKPOINT, tval: self.pc }),
                0x302 if rs1(i) == 0 => {
                    if self.prv != prv::M {
                        return Err(illegal);
                    }
                    self.mret();
                    return Ok(());
                }
                0x102 if rs1(i) == 0 => {
                    if self.prv < prv::S || (self.prv == prv::S && self.mstatus & mstatus::TSR != 0) {
                        return Err(illegal);
                    }
                    self.sret();
                    return Ok(());
                }
                0x105 if rs1(i) == 0 => {
                    // wfi
                    if self.prv < prv::M && self.mstatus & mstatus::TW != 0 || self.prv == prv::U {
                        return Err(illegal);
                    }
                    self.pc = next;
                    if !self.interrupt_pending() {
                        self.wfi = true;
                    }
                    return Ok(());
                }
                _ if f7(i) == 0x09 => {
                    // sfence.vma
                    if self.prv < prv::S || (self.prv == prv::S && self.mstatus & mstatus::TVM != 0) {
                        return Err(illegal);
                    }
                    self.tlb.flush();
                    self.invalidate_fetch();
                }
                _ => return Err(illegal),
            }
            self.pc = next;
            return Ok(());
        }
        let csr = (i >> 20) as u16;
        let src = if f & 4 != 0 { rs1(i) as u64 } else { self.x[rs1(i)] };
        let write = !(matches!(f & 3, 2 | 3) && rs1(i) == 0);
        let read = !(f & 3 == 1 && rd(i) == 0);
        let old = if read || f & 3 != 1 {
            match self.csr_read(bus, csr) {
                Some(v) => v,
                None => return Err(illegal),
            }
        } else {
            0
        };
        if write {
            // csrrs/csrrc on mip modify the software-writable SEIP bit, not the line
            // from the PLIC (privileged ISA, "Machine Interrupt Registers")
            let base = if csr == 0x344 { self.mip_sw } else { old };
            let new = match f & 3 {
                1 => src,
                2 => base | src,
                _ => base & !src,
            };
            if !self.csr_write(csr, new) {
                return Err(illegal);
            }
        }
        self.set(rd(i), old);
        self.pc = next;
        Ok(())
    }
}

fn mul_div(f: u32, a: u64, b: u64) -> u64 {
    match f {
        0 => a.wrapping_mul(b),
        1 => (((a as i64 as i128) * (b as i64 as i128)) >> 64) as u64,
        2 => (((a as i64 as i128) * (b as u128 as i128)) >> 64) as u64,
        3 => (((a as u128) * (b as u128)) >> 64) as u64,
        4 => {
            if b == 0 {
                u64::MAX
            } else {
                (a as i64).wrapping_div(b as i64) as u64
            }
        }
        5 => a.checked_div(b).unwrap_or(u64::MAX),
        6 => {
            if b == 0 {
                a
            } else {
                (a as i64).wrapping_rem(b as i64) as u64
            }
        }
        _ => {
            if b == 0 {
                a
            } else {
                a % b
            }
        }
    }
}

fn mul_div_w(f: u32, a: u32, b: u32) -> u64 {
    let r: u32 = match f {
        0 => a.wrapping_mul(b),
        4 => {
            if b == 0 {
                u32::MAX
            } else {
                (a as i32).wrapping_div(b as i32) as u32
            }
        }
        5 => a.checked_div(b).unwrap_or(u32::MAX),
        6 => {
            if b == 0 {
                a
            } else {
                (a as i32).wrapping_rem(b as i32) as u32
            }
        }
        7 => {
            if b == 0 {
                a
            } else {
                a % b
            }
        }
        _ => return 0,
    };
    sext32(r as u64)
}
