//! Scalar floating point and the Advanced SIMD subset used by common Linux userlands.
//! Single and double precision use host IEEE arithmetic (round-to-nearest-even, the reset
//! FPCR mode); FPCR rounding-mode, flush-to-zero and default-NaN controls and FPSR exception
//! flags are not modelled yet.
use crate::cpu::Cpu;
use crate::fpu::Rnd;
use emu_core::{Bus, Trap};

#[inline] fn bits(x: u32, hi: u32, lo: u32) -> u32 { (x >> lo) & ((1u32 << (hi - lo + 1)) - 1) }
#[inline] fn bit(x: u32, b: u32) -> bool { (x >> b) & 1 != 0 }

/// Element `i` of width `esize` bits.
#[inline] fn lane(v: u128, esize: u32, i: u32) -> u64 { ((v >> (esize * i)) as u64) & emask(esize) }
#[inline] fn emask(esize: u32) -> u64 { if esize >= 64 { u64::MAX } else { (1u64 << esize) - 1 } }
#[inline] fn set_lane(v: &mut u128, esize: u32, i: u32, x: u64) {
    let m = (emask(esize) as u128) << (esize * i);
    *v = (*v & !m) | (((x & emask(esize)) as u128) << (esize * i));
}
fn q_mask(q: bool, v: u128) -> u128 { if q { v } else { v & u64::MAX as u128 } }
fn replicate_lane(x: u64, esize: u32) -> u128 { replicate(x, esize) }
fn replicate(x: u64, esize: u32) -> u128 {
    let mut v = 0u128;
    for i in 0..128 / esize { set_lane(&mut v, esize, i, x); }
    v
}

/// VFPExpandImm
fn vfp_imm(imm8: u32, double: bool) -> u64 {
    let sign = (imm8 >> 7) as u64;
    let b6 = ((imm8 >> 6) & 1) as u64;
    let low6 = (imm8 & 0x3f) as u64;
    if double {
        let exp = ((1 - b6) << 10) | (if b6 == 1 { 0xff } else { 0 } << 2) | ((imm8 as u64 >> 4) & 3);
        (sign << 63) | (exp << 52) | ((low6 & 0xf) << 48)
    } else {
        let exp = ((1 - b6) << 7) | (if b6 == 1 { 0x1f } else { 0 } << 2) | ((imm8 as u64 >> 4) & 3);
        (sign << 31) | (exp << 23) | ((low6 & 0xf) << 19)
    }
}

impl Cpu {
    /// The FP unit as FPCR configures it; `fpu_done` adds its exception flags to FPSR.
    #[inline]
    pub(crate) fn fpu(&self) -> crate::fpu::Fpu { crate::fpu::Fpu::new(self.fpcr) }
    #[inline]
    pub(crate) fn fpu_done(&mut self, f: crate::fpu::Fpu) { self.fpsr |= f.flags as u64; }

    pub(crate) fn simd_fp_exec(&mut self, bus: &mut dyn Bus, i: u32) -> Result<Option<u64>, Trap> {
        let _ = bus;
        if !bit(i, 30) && bits(i, 28, 24) == 0b11110 && !bit(i, 31) && !bit(i, 29) {
            return self.fp_scalar(i);
        }
        if bits(i, 28, 24) == 0b11111 && !bit(i, 30) && !bit(i, 31) && !bit(i, 29) {
            return self.fp_3src(i);
        }
        // FMOV / conversions between general and FP registers with sf = 1
        if bits(i, 30, 24) == 0b0011110 && bit(i, 21) {
            return self.fp_scalar(i);
        }
        // conversions between FP and fixed-point integers (sf = 0 or 1)
        if bits(i, 30, 24) == 0b0011110 && !bit(i, 21) {
            return self.fp_fixed(i);
        }
        if !bit(i, 31) && bits(i, 28, 19) == 0b0111100000 && bit(i, 10) {
            return self.simd_mod_imm(i);
        }
        if !bit(i, 31) && (bits(i, 28, 24) == 0b01110 || bits(i, 28, 24) == 0b01111 || (bit(i, 30) && (bits(i, 28, 24) == 0b11110 || bits(i, 28, 24) == 0b11111))) {
            return self.neon(i);
        }
        Err(Trap::Unimplemented(self.pc, i))
    }

    // ------------------------------------------------------------------ scalar FP
    fn fp_scalar(&mut self, i: u32) -> Result<Option<u64>, Trap> {
        if !bit(i, 21) && bits(i, 30, 24) == 0b0011110 { return self.fp_fixed(i); }
        let ty = bits(i, 23, 22);
        let rd = bits(i, 4, 0);
        let rn = bits(i, 9, 5);
        let rm = bits(i, 20, 16);
        let double = ty == 1;
        if bit(i, 21) && bits(i, 15, 10) == 0 {
            // conversions between FP and integer
            let sf = bit(i, 31);
            let rmode = bits(i, 20, 19);
            let op = bits(i, 18, 16);
            match (rmode, op) {
                (0, 0b110) | (0, 0b111) | (1, 0b110) | (1, 0b111) => {
                    let to_gp = op == 0b110;
                    match (sf, ty, rmode, to_gp) {
                        (false, 0, 0, true) => { let v = self.v[rn as usize] as u32 as u64; self.set_x(rd, v); }
                        (false, 0, 0, false) => self.v[rd as usize] = self.xr(rn) as u32 as u128,
                        (true, 1, 0, true) => { let v = self.v[rn as usize] as u64; self.set_x(rd, v); }
                        (true, 1, 0, false) => self.v[rd as usize] = self.xr(rn) as u128,
                        (true, 2, 1, true) => { let v = (self.v[rn as usize] >> 64) as u64; self.set_x(rd, v); }
                        (true, 2, 1, false) => {
                            let lo = self.v[rd as usize] & u64::MAX as u128;
                            self.v[rd as usize] = lo | ((self.xr(rn) as u128) << 64);
                        }
                        _ => return Err(self.undefined()),
                    }
                    return Ok(None);
                }
                (0, 0b010) | (0, 0b011) => {
                    // SCVTF / UCVTF
                    if ty > 1 { return Err(self.unimpl_fp(i)); }
                    let mut fp = self.fpu();
                    let v = fp.from_int(if double { 64 } else { 32 }, self.xr(rn), op == 0b010, if sf { 64 } else { 32 }, 0);
                    self.fpu_done(fp);
                    self.v[rd as usize] = v as u128;
                    return Ok(None);
                }
                (_, 0b000) | (_, 0b001) | (0, 0b100) | (0, 0b101) => {
                    // FCVT{N,P,M,Z,A}{S,U}
                    if ty > 1 { return Err(self.unimpl_fp(i)); }
                    let rnd = if op & 4 != 0 { Rnd::Away } else { Rnd::from_bits(rmode as u64) };
                    let mut fp = self.fpu();
                    let es = if double { 64 } else { 32 };
                    let v = fp.to_int(es, self.v[rn as usize] as u64 & emask(es), rnd, op & 1 != 0, if sf { 64 } else { 32 }, 0);
                    self.fpu_done(fp);
                    self.set_x(rd, v);
                    return Ok(None);
                }
                _ => return Err(self.unimpl_fp(i)),
            }
        }
        if bit(i, 31) || ty == 2 { return Err(self.undefined()); }
        let es = match ty { 0 => 32, 1 => 64, _ => 16 };
        let get = |c: &Self, r: u32| c.v[r as usize] as u64 & emask(es);
        if bit(i, 21) && bits(i, 14, 10) == 0b10000 {
            // 1-source
            let opc = bits(i, 20, 15);
            let a = get(self, rn);
            let sign = 1u64 << (es - 1);
            // half precision: only the conversions exist on ARMv8.0
            if es == 16 && !matches!(opc, 4 | 5) { return Err(self.unimpl_fp(i)); }
            let mut fp = self.fpu();
            let r = match opc {
                0 => a,
                1 => a & !sign,
                2 => a ^ sign,
                3 => fp.sqrt(es, a),
                4 | 5 | 7 => {
                    let to = match opc { 4 => 32, 5 => 64, _ => 16 };
                    if to == es { return Err(self.undefined()); }
                    let r = fp.convert(es, to, a);
                    self.fpu_done(fp);
                    self.v[rd as usize] = r as u128;
                    return Ok(None);
                }
                8..=12 | 14 | 15 => {
                    let rnd = match opc { 8..=11 => Rnd::from_bits((opc - 8) as u64), 12 => Rnd::Away, _ => fp.rnd };
                    fp.round_int(es, a, rnd, opc == 14)
                }
                _ => return Err(self.unimpl_fp(i)),
            };
            self.fpu_done(fp);
            self.v[rd as usize] = r as u128;
            return Ok(None);
        }
        if es == 16 { return Err(self.unimpl_fp(i)); }
        if bit(i, 21) && bits(i, 13, 10) == 0b1000 && bits(i, 15, 14) == 0 {
            // FCMP / FCMPE (bit 4), against zero (bit 3)
            let a = get(self, rn);
            let b = if bit(i, 3) { 0 } else { get(self, rm) };
            let mut fp = self.fpu();
            self.nzcv = fp.cmp(es, a, b, bit(i, 4));
            self.fpu_done(fp);
            return Ok(None);
        }
        if bit(i, 21) && bits(i, 12, 10) == 0b100 {
            // FMOV (scalar, immediate)
            let v = vfp_imm(bits(i, 20, 13), double);
            self.v[rd as usize] = v as u128;
            return Ok(None);
        }
        if bit(i, 21) && bits(i, 11, 10) == 0b01 {
            // FCCMP / FCCMPE
            if self.cond_holds(bits(i, 15, 12)) {
                let mut fp = self.fpu();
                self.nzcv = fp.cmp(es, get(self, rn), get(self, rm), bit(i, 4));
                self.fpu_done(fp);
            } else {
                self.nzcv = bits(i, 3, 0) << 28;
            }
            return Ok(None);
        }
        if bit(i, 21) && bits(i, 11, 10) == 0b10 {
            let (a, b) = (get(self, rn), get(self, rm));
            let mut fp = self.fpu();
            let r = match bits(i, 15, 12) {
                0 => fp.mul(es, a, b),
                1 => fp.div(es, a, b),
                2 => fp.add(es, a, b),
                3 => fp.sub(es, a, b),
                4 => fp.max(es, a, b),
                5 => fp.min(es, a, b),
                6 => fp.maxnm(es, a, b),
                7 => fp.minnm(es, a, b),
                8 => fp.mul(es, a, b) ^ (1u64 << (es - 1)), // FNMUL
                _ => return Err(self.unimpl_fp(i)),
            };
            self.fpu_done(fp);
            self.v[rd as usize] = r as u128;
            return Ok(None);
        }
        if bit(i, 21) && bits(i, 11, 10) == 0b11 {
            // FCSEL
            let src = if self.cond_holds(bits(i, 15, 12)) { rn } else { rm };
            let v = if double { self.v[src as usize] as u64 as u128 } else { self.v[src as usize] as u32 as u128 };
            self.v[rd as usize] = v;
            return Ok(None);
        }
        Err(self.unimpl_fp(i))
    }

    /// SCVTF/UCVTF/FCVTZS/FCVTZU (scalar, fixed-point): fbits = 64 - scale.
    fn fp_fixed(&mut self, i: u32) -> Result<Option<u64>, Trap> {
        let sf = bit(i, 31);
        let ty = bits(i, 23, 22);
        let rmode_op = bits(i, 20, 16);
        let scale = bits(i, 15, 10);
        let (rd, rn) = (bits(i, 4, 0), bits(i, 9, 5));
        if ty > 1 || (!sf && scale < 32) { return Err(self.undefined()); }
        let fbits = 64 - scale;
        let es = if ty == 1 { 64 } else { 32 };
        let bits_i = if sf { 64 } else { 32 };
        let mut fp = self.fpu();
        match rmode_op {
            0b00010 | 0b00011 => {
                let v = fp.from_int(es, self.xr(rn), rmode_op == 0b00010, bits_i, fbits);
                self.v[rd as usize] = v as u128;
            }
            0b11000 | 0b11001 => {
                let v = fp.to_int(es, self.v[rn as usize] as u64 & emask(es), Rnd::Zero, rmode_op == 0b11001, bits_i, fbits);
                self.set_x(rd, v);
            }
            _ => return Err(self.undefined()),
        }
        self.fpu_done(fp);
        Ok(None)
    }

    fn fp_3src(&mut self, i: u32) -> Result<Option<u64>, Trap> {
        let ty = bits(i, 23, 22);
        if ty > 1 { return Err(self.unimpl_fp(i)); }
        let (rd, rn, rm, ra) = (bits(i, 4, 0), bits(i, 9, 5), bits(i, 20, 16), bits(i, 14, 10));
        // FMADD: a + n*m; FMSUB: a - n*m; FNMADD: -a - n*m; FNMSUB: -a + n*m
        let neg_prod = bit(i, 15) != bit(i, 21);
        let neg_acc = bit(i, 21);
        let es = if ty == 1 { 64 } else { 32 };
        let get = |r: u32| if ty == 1 { self.v[r as usize] as u64 } else { self.v[r as usize] as u32 as u64 };
        let sign = 1u64 << (es - 1);
        let (n, m, a) = (get(rn), get(rm), get(ra));
        let mut fp = self.fpu();
        let v = fp.fma(es, if neg_acc { a ^ sign } else { a }, if neg_prod { n ^ sign } else { n }, m);
        self.fpu_done(fp);
        self.v[rd as usize] = v as u128;
        Ok(None)
    }

    fn unimpl_fp(&self, i: u32) -> Trap { Trap::Unimplemented(self.pc, i) }

    fn simd_mod_imm(&mut self, i: u32) -> Result<Option<u64>, Trap> {
        let q = bit(i, 30);
        let op = bit(i, 29);
        let cmode = bits(i, 15, 12);
        let rd = bits(i, 4, 0) as usize;
        let imm8 = ((bits(i, 18, 16) << 5) | bits(i, 9, 5)) as u64;
        let (imm64, kind): (u64, u8) = match cmode {
            0..=7 => {
                let v = imm8 << (8 * (cmode >> 1));
                (v | v << 32, if cmode & 1 == 0 { if op { 1 } else { 0 } } else if op { 3 } else { 2 })
            }
            8..=11 => {
                let v = imm8 << (8 * ((cmode >> 1) & 1));
                let v = v | v << 16;
                (v | v << 32, if cmode & 1 == 0 { if op { 1 } else { 0 } } else if op { 3 } else { 2 })
            }
            12 | 13 => {
                let v = if cmode == 12 { (imm8 << 8) | 0xff } else { (imm8 << 16) | 0xffff };
                (v | v << 32, if op { 1 } else { 0 })
            }
            14 => {
                if !op { (u64::from_le_bytes([imm8 as u8; 8]), 0) } else {
                    let mut v = 0u64;
                    for b in 0..8 { if imm8 & (1 << b) != 0 { v |= 0xff << (8 * b); } }
                    (v, 0)
                }
            }
            _ => {
                if !op {
                    let s = vfp_imm(imm8 as u32, false);
                    (s | s << 32, 0)
                } else {
                    if !q { return Err(self.undefined()); }
                    (vfp_imm(imm8 as u32, true), 0)
                }
            }
        };
        let imm = (imm64 as u128) | ((imm64 as u128) << 64);
        let r = match kind {
            0 => imm,              // MOVI / FMOV
            1 => !imm,             // MVNI
            2 => self.v[rd] | imm, // ORR
            _ => self.v[rd] & !imm, // BIC
        };
        self.v[rd] = q_mask(q, r);
        Ok(None)
    }

    // ------------------------------------------------------------------ LD1..LD4 / ST1..ST4 (multiple structures)
    /// LD1..LD4 / ST1..ST4 (single structure, one lane) and LD1R..LD4R (replicate).
    pub(crate) fn ld_st_single(&mut self, bus: &mut dyn Bus, i: u32) -> Result<(), Trap> {
        let q = bit(i, 30);
        let post = bit(i, 23);
        let l = bit(i, 22);
        let r = bit(i, 21) as u32;
        let rm = bits(i, 20, 16);
        let opc = bits(i, 15, 13);
        let sbit = bit(i, 12) as u32;
        let size = bits(i, 11, 10);
        let rn = bits(i, 9, 5);
        let rt = bits(i, 4, 0);
        let selem = (((opc & 1) << 1) | r) + 1;
        let (scale, index, replicate) = match opc >> 1 {
            0 => (0, ((q as u32) << 3) | (sbit << 2) | size, false),
            1 => { if size & 1 != 0 { return Err(self.undefined()); } (1, ((q as u32) << 2) | (sbit << 1) | (size >> 1), false) }
            2 => match size {
                0 => (2, ((q as u32) << 1) | sbit, false),
                1 if sbit == 0 => (3, q as u32, false),
                _ => return Err(self.undefined()),
            },
            _ => { if !l || sbit != 0 { return Err(self.undefined()); } (size, 0, true) }
        };
        let es = 8u32 << scale;
        let eb = es / 8;
        let base = self.xsp(rn);
        let mut addr = base;
        let mut vals = Vec::with_capacity(selem as usize);
        for _ in 0..selem {
            if l { vals.push(self.load(bus, addr, eb, false)? as u64); }
            addr = addr.wrapping_add(eb as u64);
        }
        if !l {
            addr = base;
            for s in 0..selem {
                let v = lane(self.v[((rt + s) % 32) as usize], es, index);
                self.store(bus, addr, eb, v as u128, false)?;
                addr = addr.wrapping_add(eb as u64);
            }
        } else {
            for (s, &v) in vals.iter().enumerate() {
                let reg = ((rt + s as u32) % 32) as usize;
                if replicate { self.v[reg] = q_mask(q, replicate_lane(v, es)); } else { set_lane(&mut self.v[reg], es, index, v); }
            }
        }
        if post {
            let inc = if rm == 31 { (selem * eb) as u64 } else { self.xr(rm) };
            self.set_xsp(rn, base.wrapping_add(inc));
        }
        Ok(())
    }

    pub(crate) fn ld_st_multi(&mut self, bus: &mut dyn Bus, i: u32) -> Result<(), Trap> {
        let q = bit(i, 30);
        let post = bit(i, 23);
        let l = bit(i, 22);
        let rm = bits(i, 20, 16);
        let opc = bits(i, 15, 12);
        let size = bits(i, 11, 10);
        let rn = bits(i, 9, 5);
        let rt = bits(i, 4, 0);
        let (rpt, selem) = match opc {
            0b0000 => (1, 4), 0b0010 => (4, 1), 0b0100 => (1, 3), 0b0110 => (3, 1),
            0b0111 => (1, 1), 0b1000 => (1, 2), 0b1010 => (2, 1),
            _ => return Err(self.undefined()),
        };
        let es = 8u32 << size;
        let ebytes = es / 8;
        let elems = (if q { 128 } else { 64 }) / es;
        let base = self.xsp(rn);
        let mut addr = base;
        // stage the registers so a fault leaves them unchanged
        let mut regs: Vec<u128> = (0..rpt * selem).map(|k| self.v[((rt + k) % 32) as usize]).collect();
        if l { for r in regs.iter_mut() { *r = 0; } }
        for r in 0..rpt {
            for e in 0..elems {
                for s in 0..selem {
                    let idx = (r + s) as usize;
                    if l {
                        let v = self.load(bus, addr, ebytes, false)? as u64;
                        set_lane(&mut regs[idx], es, e, v);
                    } else {
                        let v = lane(regs[idx], es, e);
                        self.store(bus, addr, ebytes, v as u128, false)?;
                    }
                    addr = addr.wrapping_add(ebytes as u64);
                }
            }
        }
        if l { for (k, v) in regs.into_iter().enumerate() { self.v[((rt + k as u32) % 32) as usize] = v; } }
        if post {
            let inc = if rm == 31 { addr - base } else { self.xr(rm) };
            self.set_xsp(rn, base.wrapping_add(inc));
        }
        Ok(())
    }
}



