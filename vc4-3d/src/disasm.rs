//! QPU disassembly in the style of Mesa's `vc4_qpu_disasm.c`, for traces (`SBC_VC4_TRACE=N`:
//! the first N instructions of each job's shaders, with the registers they write).

use crate::qpu::{self, decode, op, sig, Inst, Kind};

const ADD_OPS: [&str; 32] = ["nop", "fadd", "fsub", "fmin", "fmax", "fminabs", "fmaxabs", "ftoi", "itof", "?9", "?10", "?11", "add", "sub", "shr",
                              "asr", "ror", "shl", "min", "max", "and", "or", "xor", "not", "clz", "?25", "?26", "?27", "?28", "?29", "v8adds", "v8subs"];
const MUL_OPS: [&str; 8] = ["nop", "fmul", "mul24", "v8muld", "v8min", "v8max", "v8adds", "v8subs"];
const SIGS: [&str; 16] = ["bkpt", "", "thrsw", "thrend", "sbwait", "sbdone", "lthrsw", "loadcv", "loadc", "ldcend", "ldtmu0", "ldtmu1", "loadam", "smimm", "loadi", "bra"];
const CONDS: [&str; 8] = [".never", "", ".zs", ".zc", ".ns", ".nc", ".cs", ".cc"];
const BCONDS: [&str; 16] = [".all_zs", ".all_zc", ".any_zs", ".any_zc", ".all_ns", ".all_nc", ".any_ns", ".any_nc",
                            ".all_cs", ".all_cc", ".any_cs", ".any_cc", ".?12", ".?13", ".?14", ""];
const PACK_A: [&str; 16] = ["", ".16a", ".16b", ".8888", ".8a", ".8b", ".8c", ".8d", ".sat", ".16a.sat", ".16b.sat", ".8888.sat", ".8a.sat", ".8b.sat", ".8c.sat", ".8d.sat"];
const PACK_MUL: [&str; 8] = ["", ".?1", ".?2", ".8888c", ".8ac", ".8bc", ".8cc", ".8dc"];
const UNPACK: [&str; 8] = ["", ".16a", ".16b", ".8d_rep", ".8a", ".8b", ".8c", ".8d"];

/// A write address as text (`a` = the A side).
pub fn waddr(w: u8, a: bool) -> String {
    match w {
        0..=31 => format!("r{}{w}", if a { 'a' } else { 'b' }),
        32..=35 => format!("r{}", w - 32),
        36 => "tmu_noswap".into(),
        37 => if a { "r5quad".into() } else { "r5rep".into() },
        38 => "host_int".into(),
        39 => "-".into(),
        40 => "uniforms_addr".into(),
        41 => if a { "quad_x".into() } else { "quad_y".into() },
        42 => if a { "ms_flags".into() } else { "rev_flag".into() },
        43 => "tlb_stencil_setup".into(),
        44 => "tlb_z".into(),
        45 => "tlb_color_ms".into(),
        46 => "tlb_color_all".into(),
        47 => "tlb_alpha_mask".into(),
        48 => "vpm".into(),
        49 => if a { "vr_setup".into() } else { "vw_setup".into() },
        50 => if a { "vr_addr".into() } else { "vw_addr".into() },
        51 => "mutex_release".into(),
        52 => "sfu_recip".into(),
        53 => "sfu_recipsqrt".into(),
        54 => "sfu_exp".into(),
        55 => "sfu_log".into(),
        56..=63 => format!("tmu{}_{}", (w - 56) / 4, ["s", "t", "r", "b"][((w - 56) % 4) as usize]),
        _ => format!("w{w}"),
    }
}

fn raddr(r: u8, b: bool) -> String {
    match r {
        0..=31 => format!("r{}{r}", if b { 'b' } else { 'a' }),
        qpu::raddr::UNIF => "unif".into(),
        qpu::raddr::VARY => "vary".into(),
        qpu::raddr::ELEM_QPU => if b { "qpu_num".into() } else { "elem_num".into() },
        qpu::raddr::NOP => "-".into(),
        qpu::raddr::XY_PIXEL => if b { "y_pix".into() } else { "x_pix".into() },
        qpu::raddr::MS_REV => if b { "rev_flag".into() } else { "ms_flags".into() },
        qpu::raddr::VPM => "vpm".into(),
        qpu::raddr::VPM_BUSY => if b { "vw_busy".into() } else { "vr_busy".into() },
        qpu::raddr::VPM_WAIT => if b { "vw_wait".into() } else { "vr_wait".into() },
        qpu::raddr::MUTEX => "mutex".into(),
        _ => format!("?{r}"),
    }
}

fn mux(i: &Inst, m: u8, is_mul: bool) -> String {
    let mut s = match m {
        0..=5 => format!("r{m}"),
        6 => raddr(i.raddr_a, false),
        _ => if i.sig == sig::SMALL_IMM {
            let n = i.raddr_b;
            match n {
                0..=31 => format!("{}", n as i8 - if n >= 16 { 32 } else { 0 }),
                32..=47 => format!("{:?}", f32::from_bits(qpu::small_imm(n))),
                48 => if is_mul { "r5_rot".into() } else { "?".into() },
                _ => if is_mul { format!("rot{}", n - 48) } else { "?".into() },
            }
        } else { raddr(i.raddr_b, true) },
    };
    if i.unpack != 0 && ((!i.pm && m == 6) || (i.pm && m == 4)) { s.push_str(UNPACK[i.unpack as usize]); }
    s
}

/// One instruction as text.
pub fn disasm(w: u64) -> String {
    let i = decode(w);
    match i.kind {
        Kind::Branch => {
            let mut s = format!("brr{}", BCONDS[i.bcond as usize]);
            if !i.rel { s = format!("bra{}", BCONDS[i.bcond as usize]); }
            s.push_str(&format!(" {}, {}", waddr(i.waddr_add, !i.ws), waddr(i.waddr_mul, i.ws)));
            s.push_str(&format!(", {:#x}", i.imm));
            if i.reg { s.push_str(&format!(" + ra{}", i.raddr_a)); }
            s
        }
        Kind::Ldi(_) | Kind::Sem(_, _) => {
            let mode = if let Kind::Ldi(m) = i.kind { m } else { 0 };
            let mut s = match i.kind {
                Kind::Sem(n, down) => format!("sem{} {n}; ", if down { "acq" } else { "rel" }),
                _ => String::new(),
            };
            let what = match mode { 1 => "load_imm_i2", 3 => "load_imm_u2", _ => "load_imm" };
            let cond = |w: u8, c: u8| if w == qpu::waddr::NOP { "" } else { CONDS[c as usize] };
            s.push_str(&format!("{what}{} {}{}, {}{}, {:#x}", if i.sf { ".sf" } else { "" },
                waddr(i.waddr_add, !i.ws), cond(i.waddr_add, i.cond_add), waddr(i.waddr_mul, i.ws), cond(i.waddr_mul, i.cond_mul), i.imm));
            s
        }
        _ => {
            let mut s = String::new();
            if i.op_add == op::A_NOP && i.op_mul == op::M_NOP {
                s.push_str("nop");
            } else {
                if i.op_add != op::A_NOP {
                    let pk = if !i.pm && !i.ws { PACK_A[i.pack as usize] } else { "" };
                    s.push_str(&format!("{}{}{} {}{}, {}, {}", ADD_OPS[i.op_add as usize], CONDS[i.cond_add as usize], if i.sf { ".sf" } else { "" },
                                        waddr(i.waddr_add, !i.ws), pk, mux(&i, i.add_a, false), mux(&i, i.add_b, false)));
                } else { s.push_str("nop"); }
                if i.op_mul != op::M_NOP {
                    let pk = if i.pm { PACK_MUL[(i.pack & 7) as usize] } else if i.ws { PACK_A[i.pack as usize] } else { "" };
                    s.push_str(&format!("; {}{} {}{}, {}, {}", MUL_OPS[i.op_mul as usize], CONDS[i.cond_mul as usize],
                                        waddr(i.waddr_mul, i.ws), pk, mux(&i, i.mul_a, true), mux(&i, i.mul_b, true)));
                }
            }
            if i.sig != sig::NONE && i.sig != sig::SMALL_IMM { s.push_str(&format!("; {}", SIGS[i.sig as usize])); }
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asm::*;

    #[test]
    fn readable() {
        assert_eq!(disasm(nop().0), "nop");
        assert_eq!(disasm(nop().add(op::FADD, 3, R0, A).ra(raddr_n(15)).0), "fadd ra3, r0, ra15");
        assert_eq!(disasm(nop().mul(op::FMUL, qpu::waddr::R1, R4, B).rb(qpu::raddr::VARY).0), "nop; fmul r1, r4, vary");
        assert_eq!(disasm(nop().mul(op::V8MIN, 0, R1, R1).pm().pack(4).0), "nop; v8min rb0.8ac, r1, r1");
        assert_eq!(disasm(nop().sig(sig::PROG_END).0), "nop; thrend");
        assert_eq!(disasm(ldi(qpu::waddr::VPM_SETUP, 0x1a00).ws().0), "load_imm vw_setup, -, 0x1a00");
        assert!(disasm(branch(15, true, 16).0).starts_with("brr "));
    }

    fn raddr_n(n: u8) -> u8 { n }
}
