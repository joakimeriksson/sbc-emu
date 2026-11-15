//! A64 decode + execute, one instruction per call. Encodings follow the Arm ARM (DDI 0487)
//! "A64 instruction set encoding" tables; group comments name the table.
use crate::cpu::UT;
use crate::cpu::{Cpu, Exc};
use crate::mmu::Access;
use emu_core::{Bus, Trap};

#[inline]
fn bits(x: u32, hi: u32, lo: u32) -> u32 {
    (x >> lo) & ((1u32 << (hi - lo + 1)) - 1)
}
#[inline]
fn bit(x: u32, b: u32) -> bool {
    (x >> b) & 1 != 0
}
#[inline]
fn sext(v: u64, width: u32) -> u64 {
    let s = 64 - width;
    (((v << s) as i64) >> s) as u64
}
#[inline]
fn ones(n: u32) -> u64 {
    if n >= 64 {
        u64::MAX
    } else {
        (1u64 << n) - 1
    }
}
#[inline]
fn mask(sf: bool, v: u64) -> u64 {
    if sf {
        v
    } else {
        v & 0xffff_ffff
    }
}

/// N Z C V flags of a result.
fn nz(sf: bool, r: u64) -> u32 {
    let n = if sf { r >> 63 } else { (r >> 31) & 1 } as u32;
    let z = (mask(sf, r) == 0) as u32;
    (n << 31) | (z << 30)
}

pub(crate) fn add_with_carry(sf: bool, x: u64, y: u64, carry: bool) -> (u64, u32) {
    if sf {
        let (r1, c1) = x.overflowing_add(y);
        let (r, c2) = r1.overflowing_add(carry as u64);
        let v = (((x ^ r) & (y ^ r)) >> 63) as u32;
        (r, nz(true, r) | (((c1 | c2) as u32) << 29) | (v << 28))
    } else {
        let (x, y) = (x & 0xffff_ffff, y & 0xffff_ffff);
        let sum = x + y + carry as u64;
        let r = sum & 0xffff_ffff;
        let c = (sum >> 32) as u32 & 1;
        let v = ((((x ^ r) & (y ^ r)) >> 31) & 1) as u32;
        (r, nz(false, r) | (c << 29) | (v << 28))
    }
}

/// DecodeBitMasks for logical immediates: returns wmask, or None if reserved.
pub fn decode_bitmask(sf: bool, n: u32, imms: u32, immr: u32) -> Option<u64> {
    let combined = (n << 6) | (!imms & 0x3f);
    if combined == 0 {
        return None;
    }
    let len = 31 - combined.leading_zeros();
    if len < 1 || (!sf && len > 5) {
        return None;
    }
    let levels = (1u32 << len) - 1;
    let s = imms & levels;
    let r = immr & levels;
    if s == levels {
        return None;
    }
    let esize = 1u32 << len;
    let welem = ones(s + 1);
    let rot = if r == 0 {
        welem
    } else {
        ((welem >> r) | (welem << (esize - r))) & ones(esize)
    };
    let mut out = 0u64;
    let mut i = 0;
    while i < 64 {
        out |= rot << i;
        i += esize;
    }
    Some(mask(sf, out))
}

fn shift_reg(sf: bool, v: u64, ty: u32, amt: u32) -> u64 {
    let w = if sf { 64 } else { 32 };
    let v = mask(sf, v);
    let amt = amt % w;
    mask(
        sf,
        match ty {
            0 => v << amt,
            1 => v >> amt,
            2 => {
                if sf {
                    ((v as i64) >> amt) as u64
                } else {
                    (((v as u32) as i32) >> amt) as u32 as u64
                }
            }
            _ => {
                if amt == 0 {
                    v
                } else {
                    (v >> amt) | (v << (w - amt))
                }
            }
        },
    )
}

fn extend_reg(v: u64, option: u32, shift: u32) -> u64 {
    let e = match option {
        0 => v & 0xff,
        1 => v & 0xffff,
        2 => v & 0xffff_ffff,
        3 => v,
        4 => sext(v & 0xff, 8),
        5 => sext(v & 0xffff, 16),
        6 => sext(v & 0xffff_ffff, 32),
        _ => v,
    };
    e << shift
}

impl Cpu {
    pub(crate) fn cond_holds(&self, cond: u32) -> bool {
        let f = self.nzcv >> 28;
        let (n, z, c, v) = (f & 8 != 0, f & 4 != 0, f & 2 != 0, f & 1 != 0);
        let r = match cond >> 1 {
            0 => z,
            1 => c,
            2 => n,
            3 => v,
            4 => c && !z,
            5 => n == v,
            6 => n == v && !z,
            _ => true,
        };
        if cond & 1 == 1 && cond != 15 {
            !r
        } else {
            r
        }
    }

    // ------------------------------------------------------------------ memory
    fn abort(&mut self, va: u64, write: bool, fsc: u32, fetch: bool) -> Trap {
        let lower = self.el == 0;
        let ec = match (fetch, lower) {
            (true, true) => Exc::IABT_LOWER,
            (true, false) => Exc::IABT_SAME,
            (false, true) => Exc::DABT_LOWER,
            (false, false) => Exc::DABT_SAME,
        };
        let iss = fsc | if write && !fetch { 1 << 6 } else { 0 };
        self.sync_exception(ec, iss, self.pc, Some(va))
    }

    fn pa(&mut self, bus: &mut dyn Bus, va: u64, acc: Access) -> Result<u64, Trap> {
        match self.translate(bus, va, acc) {
            Ok(pa) => Ok(pa),
            Err(fsc) => Err(self.abort(va, acc.is_write(), fsc, acc == Access::Exec)),
        }
    }

    /// A debugger watchpoint covering [va, va + size) for an access of `kind` (1 write, 2 read).
    #[cold]
    fn watch_check(&mut self, va: u64, size: u32, kind: u8) {
        if self.watch_hit.is_some() { return; }
        if let Some(&(a, _, _)) = self.watch.iter().find(|&&(a, n, k)| k & kind != 0 && va < a.wrapping_add(n) && a < va.wrapping_add(size as u64)) {
            self.watch_hit = Some((a, kind));
        }
    }

    /// Load `size` bytes (1..=16) little-endian.
    pub(crate) fn load(
        &mut self,
        bus: &mut dyn Bus,
        va: u64,
        size: u32,
        unpriv: bool,
    ) -> Result<u128, Trap> {
        if !self.watch.is_empty() { self.watch_check(va, size, 2); }
        // unprivileged (LDTR) at EL1 goes through the EL0 entries, which exist only for pages EL0
        // may read, while they belong to the current ASID (see `utlb_unpriv`)
        let fast = !unpriv || (self.el <= 1 && self.utlb_unpriv == 0);
        if fast && size <= 16 && (va & 0xfff) + size as u64 <= 0x1000 {
            let el = if unpriv { 0 } else { self.el as u64 };
            let tag = ((va >> 12) + 1) | (el << 60);
            let ti = (va >> 12) as usize & (UT - 1);
            if self.utlb[1][ti].0 != tag {
                let pa = self.pa(bus, va, if unpriv { Access::ReadUnpriv } else { Access::Read })?;
                if self.ram.is_null() || (pa | 0xfff) >= self.ram_len {
                    return self.load_slow(bus, va, size, unpriv);
                }
                self.utlb[1][ti] = (tag, pa & !0xfff);
            }
            // SAFETY: the page lies inside guest RAM (checked when the entry was filled)
            let p = unsafe { self.ram.add((self.utlb[1][ti].1 | (va & 0xfff)) as usize) };
            let v = unsafe {
                match size {
                    1 => *p as u64,
                    2 => (p as *const u16).read_unaligned() as u64,
                    4 => (p as *const u32).read_unaligned() as u64,
                    8 => (p as *const u64).read_unaligned(),
                    _ => return Ok((p as *const u128).read_unaligned()),
                }
            };
            return Ok(v as u128);
        }
        self.load_slow(bus, va, size, unpriv)
    }

    fn load_slow(
        &mut self,
        bus: &mut dyn Bus,
        va: u64,
        size: u32,
        unpriv: bool,
    ) -> Result<u128, Trap> {
        // JIT verify mode: a device read may have side effects, the block must not be re-run
        if let Some(log) = self.store_log.as_mut() { log.push((usize::MAX, Vec::new())); }
        self.chain_stop = true;
        let acc = if unpriv {
            Access::ReadUnpriv
        } else {
            Access::Read
        };
        let crosses = (va & 0xfff) + size as u64 > 0x1000;
        if !crosses && size <= 8 && va % size as u64 == 0 {
            let pa = self.pa(bus, va, acc)?;
            let r = match size {
                1 => bus.read8(pa).map(|v| v as u64),
                2 => bus.read16(pa).map(|v| v as u64),
                4 => bus.read32(pa).map(|v| v as u64),
                _ => bus.read64(pa),
            };
            return r
                .map(|v| v as u128)
                .map_err(|_| self.abort(va, false, 0x10, false));
        }
        let mut out = 0u128;
        for i in 0..size as u64 {
            let a = va.wrapping_add(i);
            let pa = self.pa(bus, a, acc)?;
            let b = bus
                .read8(pa)
                .map_err(|_| self.abort(a, false, 0x10, false))?;
            out |= (b as u128) << (8 * i);
        }
        Ok(out)
    }

    pub(crate) fn store(
        &mut self,
        bus: &mut dyn Bus,
        va: u64,
        size: u32,
        v: u128,
        unpriv: bool,
    ) -> Result<(), Trap> {
        if !self.watch.is_empty() { self.watch_check(va, size, 1); }
        let fast = !unpriv || (self.el <= 1 && self.utlb_unpriv == 0);
        if fast && size <= 16 && (va & 0xfff) + size as u64 <= 0x1000 {
            let el = if unpriv { 0 } else { self.el as u64 };
            let tag = ((va >> 12) + 1) | (el << 60);
            let ti = (va >> 12) as usize & (UT - 1);
            if self.utlb[2][ti].0 != tag {
                let pa = self.pa(bus, va, if unpriv { Access::WriteUnpriv } else { Access::Write })?;
                if self.ram.is_null() || (pa | 0xfff) >= self.ram_len {
                    return self.store_slow(bus, va, size, v, unpriv);
                }
                self.utlb[2][ti] = (tag, pa & !0xfff);
            }
            if let Some(m) = self.excl {
                if m & !63 == va & !63 {
                    self.set_excl(None);
                }
            }
            // SAFETY: the page lies inside guest RAM (checked when the entry was filled)
            let off = (self.utlb[2][ti].1 | (va & 0xfff)) as usize;
            let p = unsafe { self.ram.add(off) };
            if let Some(log) = self.store_log.as_mut() {
                // JIT verify mode: remember the bytes this store overwrites
                let old = unsafe { std::slice::from_raw_parts(p, size as usize) }.to_vec();
                log.push((off, old));
            }
            unsafe {
                match size {
                    1 => *p = v as u8,
                    2 => (p as *mut u16).write_unaligned(v as u16),
                    4 => (p as *mut u32).write_unaligned(v as u32),
                    8 => (p as *mut u64).write_unaligned(v as u64),
                    _ => (p as *mut u128).write_unaligned(v),
                }
            }
            return Ok(());
        }
        self.store_slow(bus, va, size, v, unpriv)
    }

    fn store_slow(
        &mut self,
        bus: &mut dyn Bus,
        va: u64,
        size: u32,
        v: u128,
        unpriv: bool,
    ) -> Result<(), Trap> {
        if let Some(log) = self.store_log.as_mut() { log.push((usize::MAX, Vec::new())); } // not undoable
        self.chain_stop = true;
        let acc = if unpriv {
            Access::WriteUnpriv
        } else {
            Access::Write
        };
        let crosses = (va & 0xfff) + size as u64 > 0x1000;
        if let Some(m) = self.excl {
            if m & !63 == va & !63 {
                self.set_excl(None);
            }
        }
        if !crosses && size <= 8 && va % size as u64 == 0 {
            let pa = self.pa(bus, va, acc)?;
            let r = match size {
                1 => bus.write8(pa, v as u8),
                2 => bus.write16(pa, v as u16),
                4 => bus.write32(pa, v as u32),
                _ => bus.write64(pa, v as u64),
            };
            return r.map_err(|_| self.abort(va, true, 0x10, false));
        }
        // translate every byte first so a fault leaves memory unchanged
        let mut pas = [0u64; 16];
        for i in 0..size as usize {
            pas[i] = self.pa(bus, va.wrapping_add(i as u64), acc)?;
        }
        for i in 0..size as usize {
            bus.write8(pas[i], (v >> (8 * i)) as u8)
                .map_err(|_| self.abort(va.wrapping_add(i as u64), true, 0x10, false))?;
        }
        Ok(())
    }

    fn fp_enabled(&mut self) -> Result<(), Trap> {
        if self.el <= 1 {
            let fpen = (self.sys.cpacr_el1 >> 20) & 3;
            let trap = match fpen {
                3 => false,
                1 => self.el == 0,
                _ => true,
            };
            if trap {
                return Err(self.sync_exception(Exc::FP, 0, self.pc, None));
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------ dispatch
    pub(crate) fn exec_one(&mut self, bus: &mut dyn Bus) -> Result<(), Trap> {
        let pc = self.pc;
        if pc & 3 != 0 {
            return Err(self.sync_exception(Exc::PC_ALIGN, 0, pc, Some(pc)));
        }
        let tag = ((pc >> 12) + 1) | ((self.el as u64) << 60);
        let ti = (pc >> 12) as usize & (UT - 1);
        let insn = if self.utlb[0][ti].0 == tag {
            // SAFETY: the page was checked to lie inside guest RAM when the entry was filled
            unsafe { (self.ram.add((self.utlb[0][ti].1 | (pc & 0xfff)) as usize) as *const u32).read_unaligned() }
        } else {
            let pa = self.pa(bus, pc, Access::Exec)?;
            if !self.ram.is_null() && (pa | 0xfff) < self.ram_len {
                self.utlb[0][ti] = (tag, pa & !0xfff);
            }
            bus.fetch(pa).map_err(|_| self.abort(pc, false, 0x10, true))?
        };
        self.cur_insn = insn;
        let op0 = bits(insn, 28, 25);
        let r = match op0 {
            0b1000 | 0b1001 => self.dp_imm(insn),
            0b1010 | 0b1011 => self.branch_sys(bus, insn),
            0b0101 | 0b1101 => self.dp_reg(insn),
            0b0100 | 0b0110 | 0b1100 | 0b1110 => self.ldst(bus, insn),
            0b0111 | 0b1111 => self.simd_fp(bus, insn),
            _ => Err(self.undefined()), // unallocated groups (incl. UDF)
        };
        match r {
            Ok(Some(npc)) => {
                self.pc = npc;
                self.instret += 1;
                Ok(())
            }
            Ok(None) => {
                self.pc = pc.wrapping_add(4);
                self.instret += 1;
                Ok(())
            }
            Err(Trap::Wait) => {
                self.pc = pc.wrapping_add(4);
                self.instret += 1;
                Err(Trap::Wait)
            }
            Err(Trap::HostCall(n)) => {
                self.pc = pc.wrapping_add(4);
                self.instret += 1;
                Err(Trap::HostCall(n))
            }
            Err(t) => Err(t),
        }
    }

    fn unimpl(&self) -> Trap {
        Trap::Unimplemented(self.pc, self.cur_insn)
    }

    // ------------------------------------------------------------------ data processing (immediate)
    fn dp_imm(&mut self, i: u32) -> Result<Option<u64>, Trap> {
        let sf = bit(i, 31);
        let rd = bits(i, 4, 0);
        let rn = bits(i, 9, 5);
        match bits(i, 25, 23) {
            0b000 | 0b001 => {
                // ADR / ADRP
                let imm = sext(((bits(i, 23, 5) << 2) | bits(i, 30, 29)) as u64, 21);
                let v = if bit(i, 31) {
                    (self.pc & !0xfff).wrapping_add(imm << 12)
                } else {
                    self.pc.wrapping_add(imm)
                };
                self.set_x(rd, v);
            }
            0b010 => {
                // add/sub immediate
                let mut imm = bits(i, 21, 10) as u64;
                if bit(i, 22) {
                    imm <<= 12;
                }
                let (sub, s) = (bit(i, 30), bit(i, 29));
                let x = self.xsp(rn);
                let (r, f) = if sub {
                    add_with_carry(sf, x, !imm, true)
                } else {
                    add_with_carry(sf, x, imm, false)
                };
                let r = mask(sf, r);
                if s {
                    self.nzcv = f;
                    self.set_x(rd, r);
                } else {
                    self.set_xsp(rd, r);
                }
            }
            0b100 => {
                // logical immediate
                let Some(imm) =
                    decode_bitmask(sf, bits(i, 22, 22), bits(i, 15, 10), bits(i, 21, 16))
                else {
                    return Err(self.undefined());
                };
                let x = self.xr(rn);
                let opc = bits(i, 30, 29);
                let r = mask(
                    sf,
                    match opc {
                        0 | 3 => x & imm,
                        1 => x | imm,
                        _ => x ^ imm,
                    },
                );
                if opc == 3 {
                    self.nzcv = nz(sf, r);
                    self.set_x(rd, r);
                } else {
                    self.set_xsp(rd, r);
                }
            }
            0b101 => {
                // move wide
                let hw = bits(i, 22, 21);
                if !sf && hw > 1 {
                    return Err(self.undefined());
                }
                let imm = (bits(i, 20, 5) as u64) << (hw * 16);
                let r = match bits(i, 30, 29) {
                    0 => mask(sf, !imm),
                    2 => imm,
                    3 => (self.xr(rd) & !(0xffffu64 << (hw * 16))) | imm,
                    _ => return Err(self.undefined()),
                };
                self.set_x(rd, mask(sf, r));
            }
            0b110 => {
                // bitfield: SBFM / BFM / UBFM
                let (immr, imms) = (bits(i, 21, 16), bits(i, 15, 10));
                let w = if sf { 64 } else { 32 };
                if bit(i, 22) != sf || immr >= w || imms >= w {
                    return Err(self.undefined());
                }
                let src = mask(sf, self.xr(rn));
                let opc = bits(i, 30, 29);
                let r = if imms >= immr {
                    let width = imms - immr + 1;
                    let field = (src >> immr) & ones(width);
                    match opc {
                        0 => mask(sf, sext(field, width)),
                        1 => (self.xr(rd) & !ones(width)) | field,
                        2 => field,
                        _ => return Err(self.undefined()),
                    }
                } else {
                    let width = imms + 1;
                    let pos = w - immr;
                    let field = (src & ones(width)) << pos;
                    match opc {
                        0 => mask(sf, sext(field, pos + width)),
                        1 => (self.xr(rd) & !(ones(width) << pos)) | field,
                        2 => field,
                        _ => return Err(self.undefined()),
                    }
                };
                self.set_x(rd, mask(sf, r));
            }
            0b111 => {
                // EXTR
                let rm = bits(i, 20, 16);
                let lsb = bits(i, 15, 10);
                let w = if sf { 64 } else { 32 };
                if bits(i, 30, 29) != 0 || bit(i, 22) != sf || lsb >= w {
                    return Err(self.undefined());
                }
                let (hi, lo) = (mask(sf, self.xr(rn)), mask(sf, self.xr(rm)));
                let r = if lsb == 0 {
                    lo
                } else {
                    (lo >> lsb) | (hi << (w - lsb))
                };
                self.set_x(rd, mask(sf, r));
            }
            _ => return Err(self.undefined()),
        }
        Ok(None)
    }

    // ------------------------------------------------------------------ branches, exceptions, system
    fn branch_sys(&mut self, bus: &mut dyn Bus, i: u32) -> Result<Option<u64>, Trap> {
        let pc = self.pc;
        if bits(i, 30, 26) == 0b00101 {
            // B / BL
            let off = sext((bits(i, 25, 0) as u64) << 2, 28);
            if bit(i, 31) {
                self.x[30] = pc + 4;
            }
            return Ok(Some(pc.wrapping_add(off)));
        }
        if bits(i, 31, 24) == 0b0101_0100 {
            // B.cond
            if bit(i, 4) {
                return Err(self.undefined());
            }
            let off = sext((bits(i, 23, 5) as u64) << 2, 21);
            return Ok(if self.cond_holds(bits(i, 3, 0)) {
                Some(pc.wrapping_add(off))
            } else {
                None
            });
        }
        if bits(i, 30, 25) == 0b011010 {
            // CBZ / CBNZ
            let sf = bit(i, 31);
            let v = mask(sf, self.xr(bits(i, 4, 0)));
            let off = sext((bits(i, 23, 5) as u64) << 2, 21);
            let take = (v == 0) != bit(i, 24);
            return Ok(if take {
                Some(pc.wrapping_add(off))
            } else {
                None
            });
        }
        if bits(i, 30, 25) == 0b011011 {
            // TBZ / TBNZ
            let b = (bits(i, 31, 31) << 5) | bits(i, 23, 19);
            let set = (self.xr(bits(i, 4, 0)) >> b) & 1 != 0;
            let off = sext((bits(i, 18, 5) as u64) << 2, 16);
            return Ok(if set == bit(i, 24) {
                Some(pc.wrapping_add(off))
            } else {
                None
            });
        }
        if bits(i, 31, 24) == 0b1101_0100 {
            return self.exception_gen(i).map(|_| None);
        }
        if bits(i, 31, 22) == 0b11_0101_0100 {
            return self.system(bus, i).map(|_| None);
        }
        if bits(i, 31, 25) == 0b110_1011 {
            // unconditional branch (register)
            let opc = bits(i, 24, 21);
            let rn = bits(i, 9, 5);
            if bits(i, 20, 16) != 0b11111 || bits(i, 15, 10) != 0 || bits(i, 4, 0) != 0 {
                return Err(self.undefined());
            }
            return match opc {
                0b0000 => Ok(Some(self.xr(rn))),
                0b0001 => {
                    let t = self.xr(rn);
                    self.x[30] = pc + 4;
                    Ok(Some(t))
                }
                0b0010 => Ok(Some(self.xr(rn))),
                0b0100 if rn == 31 => {
                    if self.el == 0 {
                        return Err(self.undefined());
                    }
                    let e = self.el as usize;
                    let (elr, spsr) = (self.elr[e], self.spsr[e]);
                    let target = ((spsr >> 2) & 3) as u8;
                    if spsr & 0x10 != 0 || target > self.el {
                        // illegal return: stay, set PSTATE.IL (approximated by undefined)
                        return Err(self.sync_exception(Exc::ILLEGAL, 0, pc, None));
                    }
                    self.set_pstate(spsr);
                    self.set_excl(None);
                    Ok(Some(elr))
                }
                _ => Err(self.undefined()),
            };
        }
        Err(self.undefined())
    }

    fn exception_gen(&mut self, i: u32) -> Result<(), Trap> {
        let imm = bits(i, 20, 5);
        let next = self.pc + 4;
        match (bits(i, 23, 21), bits(i, 4, 0)) {
            (0, 1) => Err(self.sync_exception(Exc::SVC, imm, next, None)),
            (0, 2) => {
                if self.firmware_calls_to_host && self.el > 0 { return Err(Trap::HostCall(imm as u16)); }
                if self.el == 0 {
                    return Err(self.undefined());
                }
                let target = self.el.max(2);
                Err(self.sync_exception_to(target, Exc::HVC, imm, next, None))
            }
            (0, 3) => {
                if self.firmware_calls_to_host && self.el > 0 { return Err(Trap::HostCall(imm as u16)); }
                if self.el == 0 {
                    return Err(self.undefined());
                }
                Err(self.sync_exception_to(3, Exc::SMC, imm, next, None))
            }
            (1, 0) => {
                if self.brk_to_host {
                    return Err(Trap::Brk(imm as u16));
                }
                Err(self.sync_exception(Exc::BRK, imm, self.pc, None))
            }
            (2, 0) => Err(Trap::Hlt(imm as u16)),
            _ => Err(self.undefined()),
        }
    }

    fn system(&mut self, bus: &mut dyn Bus, i: u32) -> Result<(), Trap> {
        let l = bit(i, 21);
        let op0 = bits(i, 20, 19);
        let op1 = bits(i, 18, 16);
        let crn = bits(i, 15, 12);
        let crm = bits(i, 11, 8);
        let op2 = bits(i, 7, 5);
        let rt = bits(i, 4, 0);
        match (l, op0) {
            (false, 0) => {
                match crn {
                    0b0010 => {
                        // hints
                        return match (crm << 3) | op2 {
                            0b000_0010 | 0b000_0011 => { self.last_wfe = op2 == 0b010; Err(Trap::Wait) } // WFE, WFI
                            _ => Ok(()), // NOP, YIELD, SEV, SEVL, PAC/BTI hints (not implemented: NOP)
                        };
                    }
                    0b0011 => {
                        if op2 == 0b010 {
                            self.set_excl(None);
                        } else if self.mt && matches!(op2, 0b100 | 0b101) {
                            // DSB / DMB: other cores run at the same time
                            std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
                        }
                        return Ok(());
                    }
                    0b0100 => {
                        // MSR (immediate) to PSTATE fields
                        match (op1, op2) {
                            (0, 5) => {
                                if self.el == 0 {
                                    return Err(self.undefined());
                                }
                                self.spsel = crm & 1 != 0;
                            }
                            (3, 6) => {
                                if self.el == 0 && self.sctlr[1] & (1 << 9) == 0 {
                                    return Err(self.undefined());
                                }
                                self.daif |= (crm & 0xf) << 6;
                            }
                            (3, 7) => {
                                if self.el == 0 && self.sctlr[1] & (1 << 9) == 0 {
                                    return Err(self.undefined());
                                }
                                self.daif &= !((crm & 0xf) << 6);
                            }
                            _ => {} // UAO, PAN, SSBS, DIT: not implemented (ID regs say so)
                        }
                        return Ok(());
                    }
                    _ => return Err(self.undefined()),
                }
            }
            (_, 1) => return self.sys_insn(bus, l, op1, crn, crm, op2, rt),
            (_, 2) | (_, 3) => {
                let key = crate::sysreg::key(op0, op1, crn, crm, op2);
                if !self.sysreg_accessible(op1) {
                    return Err(self.sync_exception(Exc::SYSREG, 0, self.pc, None));
                }
                if l {
                    let v = self.read_sysreg(key);
                    self.set_x(rt, v);
                } else {
                    let v = self.xr(rt);
                    self.write_sysreg(key, v);
                }
                return Ok(());
            }
            _ => {}
        }
        Err(self.undefined())
    }

    fn sysreg_accessible(&self, op1: u32) -> bool {
        match op1 {
            3 => true,
            0..=2 => self.el >= 1,
            4 => self.el >= 2,
            6 => self.el >= 3,
            7 => self.el >= 3,
            _ => false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn sys_insn(
        &mut self,
        bus: &mut dyn Bus,
        l: bool,
        op1: u32,
        crn: u32,
        crm: u32,
        op2: u32,
        rt: u32,
    ) -> Result<(), Trap> {
        if l {
            return Err(self.undefined());
        }
        match crn {
            7 => {
                if op1 == 3 && crm == 4 && op2 == 1 {
                    // DC ZVA: zero a 64-byte block
                    let va = self.xr(rt) & !63;
                    for k in 0..8 {
                        self.store(bus, va + 8 * k, 8, 0, false)?;
                    }
                    return Ok(());
                }
                if crm == 8 && op1 == 0 && op2 < 4 {
                    // AT S1E1{R,W}, S1E0{R,W}
                    let acc = match op2 {
                        0 => Access::Read,
                        1 => Access::Write,
                        2 => Access::ReadUnpriv,
                        _ => Access::WriteUnpriv,
                    };
                    let va = self.xr(rt);
                    self.sys.par_el1 = match self.translate(bus, va, acc) {
                        Ok(pa) => pa & 0x0000_ffff_ffff_f000,
                        Err(fsc) => 1 | ((fsc as u64) << 1),
                    };
                    return Ok(());
                }
                // IC IALLU / IC IALLUIS / IC IVAU: drop compiled code (the only instruction cache)
                if op1 == 0 && (crm == 5 || crm == 1) && op2 == 0 {
                    if let Some(j) = self.jit.as_mut() { j.flush(); }
                    self.ic_bcast = Some(u64::MAX);
                } else if op1 == 3 && crm == 5 && op2 == 1 {
                    let va = self.xr(rt);
                    let page = match self.translate(bus, va, Access::Read) { Ok(pa) => pa >> 12, Err(_) => u64::MAX };
                    // nothing was ever compiled from this page, on any core: nothing to drop
                    if page != u64::MAX && !self.code_page_used(page) { return Ok(()); }
                    self.jit_invalidate(page);
                    self.ic_bcast = Some(match self.ic_bcast { Some(p) if p != page => u64::MAX, _ => page });
                }
                // DC / IC maintenance: caches are not modelled
                if self.el == 0 && op1 != 3 {
                    return Err(self.undefined());
                }
                Ok(())
            }
            8 => {
                if self.el == 0 {
                    return Err(self.undefined());
                }
                // by-VA operations (VAE*, VAAE*, VALE*, VAALE*) drop one page, the rest everything;
                // the inner-shareable forms (CRm = 3) also reach the other cores
                let by_va = matches!(crm, 3 | 7) && op2 & 1 == 1;
                let op = if by_va {
                    let v = (self.xr(rt) & ((1 << 44) - 1)) << 12;
                    if v >> 55 & 1 != 0 { v | 0xff00_0000_0000_0000 } else { v }
                } else { u64::MAX };
                self.tlb_invalidate(op);
                if crm == 3 { self.tlbi_bcast = Some(match self.tlbi_bcast { Some(o) if o != op => u64::MAX, _ => op }); }
                Ok(())
            }
            _ => {
                if self.el == 0 {
                    return Err(self.undefined());
                }
                Ok(())
            }
        }
    }

    // ------------------------------------------------------------------ data processing (register)
    fn dp_reg(&mut self, i: u32) -> Result<Option<u64>, Trap> {
        let sf = bit(i, 31);
        let rd = bits(i, 4, 0);
        let rn = bits(i, 9, 5);
        let rm = bits(i, 20, 16);
        if !bit(i, 28) {
            if !bit(i, 24) {
                // logical (shifted register)
                let amt = bits(i, 15, 10);
                if !sf && amt >= 32 {
                    return Err(self.undefined());
                }
                let mut y = shift_reg(sf, self.xr(rm), bits(i, 23, 22), amt);
                if bit(i, 21) {
                    y = mask(sf, !y);
                }
                let x = self.xr(rn);
                let opc = bits(i, 30, 29);
                let r = mask(
                    sf,
                    match opc {
                        0 | 3 => x & y,
                        1 => x | y,
                        _ => x ^ y,
                    },
                );
                if opc == 3 {
                    self.nzcv = nz(sf, r);
                }
                self.set_x(rd, r);
            } else {
                let (sub, s) = (bit(i, 30), bit(i, 29));
                let (x, y, to_sp) = if !bit(i, 21) {
                    let amt = bits(i, 15, 10);
                    let ty = bits(i, 23, 22);
                    if ty == 3 || (!sf && amt >= 32) {
                        return Err(self.undefined());
                    }
                    (self.xr(rn), shift_reg(sf, self.xr(rm), ty, amt), false)
                } else {
                    let sh = bits(i, 12, 10);
                    if sh > 4 || bits(i, 23, 22) != 0 {
                        return Err(self.undefined());
                    }
                    (
                        self.xsp(rn),
                        extend_reg(self.xr(rm), bits(i, 15, 13), sh),
                        !s,
                    )
                };
                let (r, f) = if sub {
                    add_with_carry(sf, x, !y, true)
                } else {
                    add_with_carry(sf, x, y, false)
                };
                let r = mask(sf, r);
                if s {
                    self.nzcv = f;
                }
                if to_sp {
                    self.set_xsp(rd, r)
                } else {
                    self.set_x(rd, r)
                }
            }
            return Ok(None);
        }
        match bits(i, 24, 21) {
            0b0000 => {
                // ADC / SBC
                if bits(i, 15, 10) != 0 {
                    return Err(self.undefined());
                }
                let c = self.nzcv & (1 << 29) != 0;
                let x = self.xr(rn);
                let y = if bit(i, 30) {
                    !self.xr(rm)
                } else {
                    self.xr(rm)
                };
                let (r, f) = add_with_carry(sf, x, y, c);
                if bit(i, 29) {
                    self.nzcv = f;
                }
                self.set_x(rd, mask(sf, r));
            }
            0b0010 => {
                // CCMN / CCMP
                if !bit(i, 29) || bit(i, 10) || bit(i, 4) {
                    return Err(self.undefined());
                }
                if self.cond_holds(bits(i, 15, 12)) {
                    let x = self.xr(rn);
                    let y = if bit(i, 11) { rm as u64 } else { self.xr(rm) };
                    let (_, f) = if bit(i, 30) {
                        add_with_carry(sf, x, !y, true)
                    } else {
                        add_with_carry(sf, x, y, false)
                    };
                    self.nzcv = f;
                } else {
                    self.nzcv = bits(i, 3, 0) << 28;
                }
            }
            0b0100 => {
                // CSEL / CSINC / CSINV / CSNEG
                if bit(i, 29) || bit(i, 11) {
                    return Err(self.undefined());
                }
                let r = if self.cond_holds(bits(i, 15, 12)) {
                    self.xr(rn)
                } else {
                    let y = self.xr(rm);
                    match (bit(i, 30), bit(i, 10)) {
                        (false, false) => y,
                        (false, true) => y.wrapping_add(1),
                        (true, false) => !y,
                        (true, true) => y.wrapping_neg(),
                    }
                };
                self.set_x(rd, mask(sf, r));
            }
            0b0110 => {
                if bit(i, 29) {
                    return Err(self.undefined());
                }
                let r = if !bit(i, 30) {
                    let x = mask(sf, self.xr(rn));
                    let y = mask(sf, self.xr(rm));
                    let w = if sf { 64 } else { 32 };
                    match bits(i, 15, 10) {
                        0b000010 => {
                            if y == 0 {
                                0
                            } else {
                                x / y
                            }
                        }
                        0b000011 => {
                            if y == 0 {
                                0
                            } else if sf {
                                (x as i64).wrapping_div(y as i64) as u64
                            } else {
                                (x as u32 as i32).wrapping_div(y as u32 as i32) as u32 as u64
                            }
                        }
                        0b001000 => shift_reg(sf, x, 0, (y % w) as u32),
                        0b001001 => shift_reg(sf, x, 1, (y % w) as u32),
                        0b001010 => shift_reg(sf, x, 2, (y % w) as u32),
                        0b001011 => shift_reg(sf, x, 3, (y % w) as u32),
                        // CRC32B/H/W/X and CRC32CB/H/W/X: sz 3 needs sf = 1, the others sf = 0
                        op @ 0b010000..=0b010111 if (op & 3 == 3) == sf => {
                            let n = 8 << (op & 3);
                            let poly = if op & 4 != 0 { 0x82f6_3b78 } else { 0xedb8_8320 };
                            crc32(self.xr(rn) as u32, self.xr(rm), n, poly) as u64
                        }
                        _ => return Err(self.undefined()),
                    }
                } else {
                    if rm != 0 {
                        return Err(self.undefined());
                    }
                    let x = mask(sf, self.xr(rn));
                    match (bits(i, 15, 10), sf) {
                        (0, true) => x.reverse_bits(),
                        (0, false) => (x as u32).reverse_bits() as u64,
                        (1, _) => {
                            let mut r = 0u64;
                            for h in 0..4 {
                                let half = (x >> (16 * h)) as u16;
                                r |= (half.swap_bytes() as u64) << (16 * h);
                            }
                            mask(sf, r)
                        }
                        (2, true) => {
                            ((x as u32).swap_bytes() as u64)
                                | (((x >> 32) as u32).swap_bytes() as u64) << 32
                        }
                        (2, false) => (x as u32).swap_bytes() as u64,
                        (3, true) => x.swap_bytes(),
                        (4, true) => x.leading_zeros() as u64,
                        (4, false) => (x as u32).leading_zeros() as u64,
                        (5, true) => {
                            ((x ^ (((x as i64) >> 1) as u64)) & !(1u64 << 63)).leading_zeros()
                                as u64
                                - 1
                        }
                        (5, false) => {
                            let v = x as u32;
                            ((v ^ ((v as i32 >> 1) as u32)) & !(1u32 << 31)).leading_zeros() as u64
                                - 1
                        }
                        _ => return Err(self.undefined()),
                    }
                };
                self.set_x(rd, r);
            }
            op if op & 0b1000 != 0 => {
                // data processing, 3 source
                let ra = bits(i, 14, 10);
                let o0 = bit(i, 15);
                let (x, y, a) = (self.xr(rn), self.xr(rm), self.xr(ra));
                if bits(i, 30, 29) != 0 {
                    return Err(self.undefined());
                }
                let r = match (bits(i, 23, 21), sf) {
                    (0, _) => {
                        let p = x.wrapping_mul(y);
                        if o0 {
                            a.wrapping_sub(p)
                        } else {
                            a.wrapping_add(p)
                        }
                    }
                    (1, true) => {
                        let p = (x as i32 as i64).wrapping_mul(y as i32 as i64) as u64;
                        if o0 {
                            a.wrapping_sub(p)
                        } else {
                            a.wrapping_add(p)
                        }
                    }
                    (5, true) => {
                        let p = (x as u32 as u64).wrapping_mul(y as u32 as u64);
                        if o0 {
                            a.wrapping_sub(p)
                        } else {
                            a.wrapping_add(p)
                        }
                    }
                    (2, true) if !o0 => (((x as i64 as i128) * (y as i64 as i128)) >> 64) as u64,
                    (6, true) if !o0 => (((x as u128) * (y as u128)) >> 64) as u64,
                    _ => return Err(self.undefined()),
                };
                self.set_x(rd, mask(sf, r));
            }
            _ => return Err(self.undefined()),
        }
        Ok(None)
    }

    // ------------------------------------------------------------------ loads and stores
    fn ldst(&mut self, bus: &mut dyn Bus, i: u32) -> Result<Option<u64>, Trap> {
        let rt = bits(i, 4, 0);
        let rn = bits(i, 9, 5);
        let vec = bit(i, 26);
        if vec {
            self.fp_enabled()?;
        }
        if vec && !bit(i, 31) && bits(i, 29, 24) == 0b001100 {
            return self.ld_st_multi(bus, i).map(|_| None);
        }
        if vec && !bit(i, 31) && bits(i, 29, 24) == 0b001101 {
            return self.ld_st_single(bus, i).map(|_| None);
        }

        // load/store exclusive and ordered
        if bits(i, 29, 24) == 0b001000 {
            if vec {
                return Err(self.undefined());
            }
            return self.ldst_excl(bus, i).map(|_| None);
        }
        // load register (literal)
        if bits(i, 29, 27) == 0b011 && bits(i, 25, 24) == 0 {
            let off = sext((bits(i, 23, 5) as u64) << 2, 21);
            let addr = self.pc.wrapping_add(off);
            let opc = bits(i, 31, 30);
            if vec {
                let size = match opc {
                    0 => 4,
                    1 => 8,
                    2 => 16,
                    _ => return Err(self.undefined()),
                };
                let v = self.load(bus, addr, size, false)?;
                self.v[rt as usize] = v;
            } else {
                match opc {
                    0 => {
                        let v = self.load(bus, addr, 4, false)?;
                        self.set_x(rt, v as u64);
                    }
                    1 => {
                        let v = self.load(bus, addr, 8, false)?;
                        self.set_x(rt, v as u64);
                    }
                    2 => {
                        let v = self.load(bus, addr, 4, false)?;
                        self.set_x(rt, sext(v as u64, 32));
                    }
                    _ => {} // PRFM literal
                }
            }
            return Ok(None);
        }
        // load/store pair
        if bits(i, 29, 27) == 0b101 {
            return self.ldst_pair(bus, i).map(|_| None);
        }
        // load/store register
        if bits(i, 29, 27) == 0b111 {
            return self.ldst_reg(bus, i).map(|_| None);
        }
        let _ = rn;
        Err(self.unimpl())
    }

    fn ldst_excl(&mut self, bus: &mut dyn Bus, i: u32) -> Result<(), Trap> {
        let size_log = bits(i, 31, 30);
        let o2 = bit(i, 23);
        let l = bit(i, 22);
        let o1 = bit(i, 21);
        let rs = bits(i, 20, 16);
        let rt2 = bits(i, 14, 10);
        let rn = bits(i, 9, 5);
        let rt = bits(i, 4, 0);
        let addr = self.xsp(rn);
        let size = 1u32 << size_log;
        if o2 {
            if o1 {
                return Err(self.unimpl());
            } // CAS (LSE): not advertised
              // LDAR / STLR / LDLAR / STLLR: ordinary accesses on one core; with cores running in
              // parallel the acquire/release order is a host fence
            use std::sync::atomic::{fence, Ordering};
            if l {
                let v = self.load(bus, addr, size, false)?;
                if self.mt { fence(Ordering::SeqCst); }
                self.set_x(rt, v as u64);
            } else {
                let v = self.xr(rt) as u128;
                if self.mt { fence(Ordering::SeqCst); }
                self.store(bus, addr, size, v, false)?;
            }
            return Ok(());
        }
        if !o1 {
            if l {
                let v = self.load(bus, addr, size, false)?;
                self.set_excl(Some(addr));
                self.excl_val = v;
                if self.mt { std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst); }
                self.set_x(rt, v as u64);
            } else {
                let held = self.excl.is_some_and(|m| m & !63 == addr & !63);
                let v = self.xr(rt) as u128;
                let ok = if !held { false } else if self.mt { self.store_cas(bus, addr, size, self.excl_val, v)? } else { self.store(bus, addr, size, v, false)?; true };
                self.set_excl(None);
                self.set_x(rs, (!ok) as u64);
            }
        } else {
            // LDXP / STXP (32- or 64-bit elements)
            if size_log < 2 {
                return Err(self.undefined());
            }
            if l {
                let a = self.load(bus, addr, size, false)?;
                let b = self.load(bus, addr + size as u64, size, false)?;
                self.set_excl(Some(addr));
                self.excl_val = a | (b << (8 * size));
                if self.mt { std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst); }
                self.set_x(rt, a as u64);
                self.set_x(rt2, b as u64);
            } else {
                let held = self.excl.is_some_and(|m| m & !63 == addr & !63);
                let (a, b) = (self.xr(rt) as u128, self.xr(rt2) as u128);
                let v = a | (b << (8 * size));
                let ok = if !held { false } else if self.mt { self.store_cas(bus, addr, 2 * size, self.excl_val, v)? } else { self.store(bus, addr, 2 * size, v, false)?; true };
                self.set_excl(None);
                self.set_x(rs, (!ok) as u64);
            }
        }
        Ok(())
    }

    /// Store-exclusive with other cores running at the same time: the store happens only if
    /// memory still holds what the load-exclusive read (a host compare-and-swap; 16-byte pairs
    /// take a global lock). Returns whether it stored.
    fn store_cas(&mut self, bus: &mut dyn Bus, va: u64, size: u32, old: u128, new: u128) -> Result<bool, Trap> {
        use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering::SeqCst};
        static PAIRS: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let pa = self.pa(bus, va, Access::Write)?;
        if self.ram.is_null() || pa + size as u64 > self.ram_len || pa % size as u64 != 0 {
            self.store(bus, va, size, new, false)?;
            return Ok(true);
        }
        // SAFETY: inside guest RAM and naturally aligned (the buffer is at least 16-byte aligned)
        let p = unsafe { self.ram.add(pa as usize) };
        let ok = unsafe {
            match size {
                1 => AtomicU8::from_ptr(p).compare_exchange(old as u8, new as u8, SeqCst, SeqCst).is_ok(),
                2 => AtomicU16::from_ptr(p as *mut u16).compare_exchange(old as u16, new as u16, SeqCst, SeqCst).is_ok(),
                4 => AtomicU32::from_ptr(p as *mut u32).compare_exchange(old as u32, new as u32, SeqCst, SeqCst).is_ok(),
                8 => AtomicU64::from_ptr(p as *mut u64).compare_exchange(old as u64, new as u64, SeqCst, SeqCst).is_ok(),
                _ => {
                    let _g = PAIRS.lock().unwrap_or_else(|e| e.into_inner());
                    let cur = (p as *const u128).read_volatile();
                    if cur == old { (p as *mut u128).write_volatile(new); true } else { false }
                }
            }
        };
        if ok {
            // the store went around the micro-TLB: keep JIT verify logs and our own monitor right
            self.chain_stop = true;
        }
        Ok(ok)
    }

    fn ldst_pair(&mut self, bus: &mut dyn Bus, i: u32) -> Result<(), Trap> {
        let opc = bits(i, 31, 30);
        let vec = bit(i, 26);
        let mode = bits(i, 24, 23);
        let l = bit(i, 22);
        let rt2 = bits(i, 14, 10);
        let rn = bits(i, 9, 5);
        let rt = bits(i, 4, 0);
        let (scale, signed) = if vec {
            if opc == 3 {
                return Err(self.undefined());
            }
            (2 + opc, false)
        } else {
            match (opc, l) {
                (0, _) => (2, false),
                (1, true) => (2, true),
                (2, _) => (3, false),
                _ => return Err(self.undefined()),
            }
        };
        let size = 1u32 << scale;
        let off = sext(bits(i, 21, 15) as u64, 7) << scale;
        let base = self.xsp(rn);
        let addr = match mode {
            1 => base,
            _ => base.wrapping_add(off),
        }; // 1 = post-index
        if l {
            let a = self.load(bus, addr, size, false)?;
            let b = self.load(bus, addr.wrapping_add(size as u64), size, false)?;
            if vec {
                self.v[rt as usize] = a;
                self.v[rt2 as usize] = b;
            } else if signed {
                self.set_x(rt, sext(a as u64, 32));
                self.set_x(rt2, sext(b as u64, 32));
            } else {
                self.set_x(rt, a as u64);
                self.set_x(rt2, b as u64);
            }
        } else {
            let (a, b) = if vec {
                (self.v[rt as usize], self.v[rt2 as usize])
            } else {
                (self.xr(rt) as u128, self.xr(rt2) as u128)
            };
            let m = if size == 16 {
                u128::MAX
            } else {
                (1u128 << (8 * size)) - 1
            };
            self.store(bus, addr, size, a & m, false)?;
            self.store(bus, addr.wrapping_add(size as u64), size, b & m, false)?;
        }
        match mode {
            1 => self.set_xsp(rn, base.wrapping_add(off)),
            3 => self.set_xsp(rn, addr),
            _ => {}
        }
        Ok(())
    }

    fn ldst_reg(&mut self, bus: &mut dyn Bus, i: u32) -> Result<(), Trap> {
        let size_log = bits(i, 31, 30);
        let vec = bit(i, 26);
        let opc = bits(i, 23, 22);
        let rn = bits(i, 9, 5);
        let rt = bits(i, 4, 0);
        // access size in bytes (log2) and kind
        let (scale, kind) = if vec {
            let s = if opc & 2 != 0 {
                if size_log != 0 {
                    return Err(self.undefined());
                }
                4
            } else {
                size_log
            };
            (s, if opc & 1 != 0 { 1 } else { 0 })
        } else {
            match (size_log, opc) {
                (_, 0) => (size_log, 0), // store
                (_, 1) => (size_log, 1), // load, zero-extend
                (3, 2) => (3, 4),        // PRFM
                (3, 3) => return Err(self.undefined()),
                (2, 3) => return Err(self.undefined()),
                (_, 2) => (size_log, 2), // load signed to 64
                (_, _) => (size_log, 3), // load signed to 32
            }
        };
        let size = 1u32 << scale;
        let base = self.xsp(rn);
        let mut unpriv = false;
        let (addr, wb) = if bit(i, 24) {
            // unsigned scaled immediate
            (base.wrapping_add((bits(i, 21, 10) as u64) << scale), None)
        } else if bit(i, 21) {
            if bits(i, 11, 10) != 0b10 {
                return Err(self.unimpl());
            } // atomic memory ops (LSE)
            let option = bits(i, 15, 13);
            if option & 2 == 0 {
                return Err(self.undefined());
            }
            let sh = if bit(i, 12) { scale } else { 0 };
            (
                base.wrapping_add(extend_reg(self.xr(bits(i, 20, 16)), option, sh)),
                None,
            )
        } else {
            let imm = sext(bits(i, 20, 12) as u64, 9);
            match bits(i, 11, 10) {
                0 => (base.wrapping_add(imm), None),
                1 => (base, Some(base.wrapping_add(imm))),
                2 => {
                    unpriv = self.el == 1;
                    (base.wrapping_add(imm), None)
                }
                _ => (base.wrapping_add(imm), Some(base.wrapping_add(imm))),
            }
        };
        match kind {
            0 => {
                let v = if vec {
                    self.v[rt as usize]
                } else {
                    self.xr(rt) as u128
                };
                let m = if size == 16 {
                    u128::MAX
                } else {
                    (1u128 << (8 * size)) - 1
                };
                self.store(bus, addr, size, v & m, unpriv)?;
            }
            4 => {} // prefetch
            _ => {
                let v = self.load(bus, addr, size, unpriv)?;
                if vec {
                    self.v[rt as usize] = v;
                } else {
                    let v = v as u64;
                    let r = match kind {
                        1 => v,
                        2 => sext(v, 8 * size),
                        _ => sext(v, 8 * size) & 0xffff_ffff,
                    };
                    self.set_x(rt, r);
                }
            }
        }
        if let Some(w) = wb {
            self.set_xsp(rn, w);
        }
        Ok(())
    }

    fn simd_fp(&mut self, bus: &mut dyn Bus, i: u32) -> Result<Option<u64>, Trap> {
        self.fp_enabled()?;
        self.simd_fp_exec(bus, i)
    }
}

/// The A64 CRC32 instructions: `bits` low bits of `data` into the bit-reflected `crc` (no inversion).
pub(crate) fn crc32(crc: u32, data: u64, bits: u32, poly: u32) -> u32 {
    let mut acc = crc;
    let mut d = data;
    for _ in 0..bits / 8 {
        acc ^= d as u8 as u32;
        for _ in 0..8 { acc = (acc >> 1) ^ (poly & (acc & 1).wrapping_neg()); }
        d >>= 8;
    }
    acc
}
