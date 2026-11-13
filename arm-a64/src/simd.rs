//! Scalar floating point and the Advanced SIMD subset used by common Linux userlands.
//! Single and double precision use host IEEE arithmetic (round-to-nearest-even, the reset
//! FPCR mode); FPCR rounding-mode, flush-to-zero and default-NaN controls and FPSR exception
//! flags are not modelled yet.
use crate::cpu::Cpu;
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

#[derive(Clone, Copy)]
enum F { S(f32), D(f64) }

impl Cpu {
    fn fread(&self, r: u32, double: bool) -> F {
        let v = self.v[r as usize];
        if double { F::D(f64::from_bits(v as u64)) } else { F::S(f32::from_bits(v as u32)) }
    }
    fn fwrite(&mut self, r: u32, f: F) {
        self.v[r as usize] = match f { F::S(x) => x.to_bits() as u128, F::D(x) => x.to_bits() as u128 };
    }

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
                    let x = self.xr(rn);
                    let unsigned = op == 0b011;
                    let val = |d: bool| -> F {
                        match (sf, unsigned, d) {
                            (true, false, true) => F::D(x as i64 as f64),
                            (true, true, true) => F::D(x as f64),
                            (false, false, true) => F::D(x as u32 as i32 as f64),
                            (false, true, true) => F::D(x as u32 as f64),
                            (true, false, false) => F::S(x as i64 as f32),
                            (true, true, false) => F::S(x as f32),
                            (false, false, false) => F::S(x as u32 as i32 as f32),
                            (false, true, false) => F::S(x as u32 as f32),
                        }
                    };
                    let v = val(double);
                    self.fwrite(rd, v);
                    return Ok(None);
                }
                (_, 0b000) | (_, 0b001) | (0, 0b100) | (0, 0b101) => {
                    // FCVT{N,P,M,Z,A}{S,U}
                    if ty > 1 { return Err(self.unimpl_fp(i)); }
                    let f = match self.fread(rn, double) { F::S(x) => x as f64, F::D(x) => x };
                    let r = match (rmode, op >> 2) {
                        (0, 0) => f.round_ties_even(),
                        (1, 0) => f.ceil(),
                        (2, 0) => f.floor(),
                        (3, 0) => f.trunc(),
                        _ => f.round(), // FCVTA*: ties away
                    };
                    let unsigned = op & 1 != 0;
                    let v = match (sf, unsigned) {
                        (true, false) => r as i64 as u64,
                        (true, true) => r as u64,
                        (false, false) => r as i32 as u32 as u64,
                        (false, true) => r as u32 as u64,
                    };
                    self.set_x(rd, v);
                    return Ok(None);
                }
                _ => return Err(self.unimpl_fp(i)),
            }
        }
        if bit(i, 31) { return Err(self.undefined()); }
        if ty > 1 { return Err(self.unimpl_fp(i)); } // half precision
        if bit(i, 21) && bits(i, 14, 10) == 0b10000 {
            // 1-source
            let opc = bits(i, 20, 15);
            let a = self.fread(rn, double);
            let r = match (opc, a) {
                (0, a) => a,
                (1, F::S(x)) => F::S(x.abs()),
                (1, F::D(x)) => F::D(x.abs()),
                (2, F::S(x)) => F::S(-x),
                (2, F::D(x)) => F::D(-x),
                (3, F::S(x)) => F::S(x.sqrt()),
                (3, F::D(x)) => F::D(x.sqrt()),
                (4, F::D(x)) => F::S(x as f32),
                (5, F::S(x)) => F::D(x as f64),
                (8..=15, a) => {
                    let rnd = |x: f64| match opc { 8 | 14 | 15 => x.round_ties_even(), 9 => x.ceil(), 10 => x.floor(), 11 => x.trunc(), _ => x.round() };
                    match a { F::S(x) => F::S(rnd(x as f64) as f32), F::D(x) => F::D(rnd(x)) }
                }
                _ => return Err(self.unimpl_fp(i)),
            };
            self.fwrite(rd, r);
            return Ok(None);
        }
        if bit(i, 21) && bits(i, 13, 10) == 0b1000 && bits(i, 15, 14) == 0 {
            // FCMP / FCMPE
            let a = self.fread(rn, double);
            let zero = bit(i, 3);
            let (x, y) = match a {
                F::S(x) => (x as f64, if zero { 0.0 } else { f32::from_bits(self.v[rm as usize] as u32) as f64 }),
                F::D(x) => (x, if zero { 0.0 } else { f64::from_bits(self.v[rm as usize] as u64) }),
            };
            self.nzcv = fcmp_flags(x, y);
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
                let (x, y) = match (self.fread(rn, double), self.fread(rm, double)) {
                    (F::S(a), F::S(b)) => (a as f64, b as f64),
                    (F::D(a), F::D(b)) => (a, b),
                    _ => unreachable!(),
                };
                self.nzcv = fcmp_flags(x, y);
            } else {
                self.nzcv = bits(i, 3, 0) << 28;
            }
            return Ok(None);
        }
        if bit(i, 21) && bits(i, 11, 10) == 0b10 {
            let es = if double { 64 } else { 32 };
            let opc = bits(i, 15, 12);
            let (ab, bb) = if double { (self.v[rn as usize] as u64, self.v[rm as usize] as u64) } else { (self.v[rn as usize] as u32 as u64, self.v[rm as usize] as u32 as u64) };
            let nm = matches!(opc, 6 | 7);
            let nan = crate::neon::process_nans(es, &[ab, bb]);
            let snan_or_both = nan.is_some() && (!nm || (f_is_nan(ab, es) && f_is_nan(bb, es)) || nan != Some(if f_is_nan(ab, es) { ab } else { bb }));
            let a = self.fread(rn, double);
            let b = self.fread(rm, double);
            let r = if let (Some(n), true) = (nan, snan_or_both) {
                if double { F::D(f64::from_bits(n)) } else { F::S(f32::from_bits(n as u32)) }
            } else if let (Some(_), true) = (nan, nm) {
                // one quiet NaN and a number: FMAXNM/FMINNM return the number
                if f_is_nan(ab, es) { b } else { a }
            } else {
                match (a, b) {
                    (F::S(x), F::S(y)) => F::S(fp2(opc, x as f64, y as f64).ok_or(Trap::Unimplemented(self.pc, i))? as f32),
                    (F::D(x), F::D(y)) => F::D(fp2(opc, x, y).ok_or(Trap::Unimplemented(self.pc, i))?),
                    _ => unreachable!(),
                }
            };
            // single-precision ops computed in f64 then rounded once: exact for + - * / and sqrt
            self.fwrite(rd, r);
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
        let double = ty == 1;
        let k = 2f64.powi(fbits as i32);
        match rmode_op {
            0b00010 | 0b00011 => {
                let x = self.xr(rn);
                let f = match (sf, rmode_op == 0b00011) {
                    (true, false) => x as i64 as f64, (true, true) => x as f64,
                    (false, false) => x as u32 as i32 as f64, (false, true) => x as u32 as f64,
                };
                // single precision: convert from the exact integer once (x/2^f is exact in binary)
                let r = if double { F::D(f / k) } else {
                    let v = match (sf, rmode_op == 0b00011) { (true, false) => x as i64 as f32, (true, true) => x as f32, (false, false) => x as u32 as i32 as f32, (false, true) => x as u32 as f32 };
                    F::S(v / k as f32)
                };
                self.fwrite(rd, r);
            }
            0b11000 | 0b11001 => {
                let f = match self.fread(rn, double) { F::S(x) => x as f64, F::D(x) => x };
                let t = (f * k).trunc();
                let v = match (sf, rmode_op == 0b11001) {
                    (true, false) => t as i64 as u64, (true, true) => t as u64,
                    (false, false) => t as i32 as u32 as u64, (false, true) => t as u32 as u64,
                };
                self.set_x(rd, v);
            }
            _ => return Err(self.undefined()),
        }
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
        let v = crate::neon::fma(es, if neg_acc { a ^ sign } else { a }, if neg_prod { n ^ sign } else { n }, m);
        let r = if ty == 1 { F::D(f64::from_bits(v)) } else { F::S(f32::from_bits(v as u32)) };
        self.fwrite(rd, r);
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

fn f_is_nan(x: u64, es: u32) -> bool { if es == 32 { f32::from_bits(x as u32).is_nan() } else { f64::from_bits(x).is_nan() } }

fn fcmp_flags(x: f64, y: f64) -> u32 {
    if x.is_nan() || y.is_nan() { 0b0011 << 28 }
    else if x == y { 0b0110 << 28 }
    else if x < y { 0b1000 << 28 }
    else { 0b0010 << 28 }
}

fn fp2(opc: u32, x: f64, y: f64) -> Option<f64> {
    Some(match opc {
        0 => x * y,
        1 => x / y,
        2 => x + y,
        3 => x - y,
        4 => if x.is_nan() || y.is_nan() { f64::NAN } else { x.max(y) },
        5 => if x.is_nan() || y.is_nan() { f64::NAN } else { x.min(y) },
        6 => x.max(y),
        7 => x.min(y),
        8 => -(x * y),
        _ => return None,
    })
}
