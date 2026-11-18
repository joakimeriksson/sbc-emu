//! RV64C: expansion of a 16-bit compressed instruction to its 32-bit equivalent
//! (The RISC-V Instruction Set Manual, "C" Standard Extension, RV64 encodings).

#[inline(always)]
fn bits(x: u32, hi: u32, lo: u32) -> u32 {
    (x >> lo) & ((1 << (hi - lo + 1)) - 1)
}

fn sext(v: u32, width: u32) -> i32 {
    ((v << (32 - width)) as i32) >> (32 - width)
}

fn i_type(imm: i32, rs1: u32, f3: u32, rd: u32, op: u32) -> u32 {
    ((imm as u32 & 0xfff) << 20) | (rs1 << 15) | (f3 << 12) | (rd << 7) | op
}

fn s_type(imm: i32, rs2: u32, rs1: u32, f3: u32, op: u32) -> u32 {
    let imm = imm as u32;
    (((imm >> 5) & 0x7f) << 25) | (rs2 << 20) | (rs1 << 15) | (f3 << 12) | ((imm & 0x1f) << 7) | op
}

fn r_type(f7: u32, rs2: u32, rs1: u32, f3: u32, rd: u32, op: u32) -> u32 {
    (f7 << 25) | (rs2 << 20) | (rs1 << 15) | (f3 << 12) | (rd << 7) | op
}

fn b_type(imm: i32, rs2: u32, rs1: u32, f3: u32) -> u32 {
    let i = imm as u32;
    (bits(i, 12, 12) << 31) | (bits(i, 10, 5) << 25) | (rs2 << 20) | (rs1 << 15) | (f3 << 12)
        | (bits(i, 4, 1) << 8) | (bits(i, 11, 11) << 7) | 0x63
}

fn j_type(imm: i32, rd: u32) -> u32 {
    let i = imm as u32;
    (bits(i, 20, 20) << 31) | (bits(i, 10, 1) << 21) | (bits(i, 11, 11) << 20) | (bits(i, 19, 12) << 12) | (rd << 7) | 0x6f
}

const LOAD: u32 = 0x03;
const LOAD_FP: u32 = 0x07;
const OP_IMM: u32 = 0x13;
const OP_IMM_32: u32 = 0x1b;
const STORE: u32 = 0x23;
const STORE_FP: u32 = 0x27;
const OP: u32 = 0x33;
const OP_32: u32 = 0x3b;
const LUI: u32 = 0x37;
const JALR: u32 = 0x67;

/// The 32-bit instruction a compressed one stands for, or 0 if it is illegal/reserved.
pub fn expand(c: u16) -> u32 {
    let c = c as u32;
    let f3 = bits(c, 15, 13);
    let rd = bits(c, 11, 7);
    let rs2 = bits(c, 6, 2);
    let rdp = bits(c, 4, 2) + 8;
    let rs1p = bits(c, 9, 7) + 8;
    let imm6 = sext(bits(c, 12, 12) << 5 | bits(c, 6, 2), 6);
    match (c & 3, f3) {
        (0, 0) => {
            let imm = bits(c, 12, 11) << 4 | bits(c, 10, 7) << 6 | bits(c, 6, 6) << 2 | bits(c, 5, 5) << 3;
            if imm == 0 {
                return 0;
            }
            i_type(imm as i32, 2, 0, rdp, OP_IMM)
        }
        (0, 1) => i_type((bits(c, 12, 10) << 3 | bits(c, 6, 5) << 6) as i32, rs1p, 3, rdp, LOAD_FP),
        (0, 2) => i_type((bits(c, 12, 10) << 3 | bits(c, 6, 6) << 2 | bits(c, 5, 5) << 6) as i32, rs1p, 2, rdp, LOAD),
        (0, 3) => i_type((bits(c, 12, 10) << 3 | bits(c, 6, 5) << 6) as i32, rs1p, 3, rdp, LOAD),
        (0, 5) => s_type((bits(c, 12, 10) << 3 | bits(c, 6, 5) << 6) as i32, rdp, rs1p, 3, STORE_FP),
        (0, 6) => s_type((bits(c, 12, 10) << 3 | bits(c, 6, 6) << 2 | bits(c, 5, 5) << 6) as i32, rdp, rs1p, 2, STORE),
        (0, 7) => s_type((bits(c, 12, 10) << 3 | bits(c, 6, 5) << 6) as i32, rdp, rs1p, 3, STORE),
        (1, 0) => i_type(imm6, rd, 0, rd, OP_IMM),
        (1, 1) => {
            if rd == 0 {
                return 0;
            }
            i_type(imm6, rd, 0, rd, OP_IMM_32)
        }
        (1, 2) => i_type(imm6, 0, 0, rd, OP_IMM),
        (1, 3) => {
            if rd == 2 {
                let imm = bits(c, 12, 12) << 9 | bits(c, 6, 6) << 4 | bits(c, 5, 5) << 6 | bits(c, 4, 3) << 7 | bits(c, 2, 2) << 5;
                if imm == 0 {
                    return 0;
                }
                i_type(sext(imm, 10), 2, 0, 2, OP_IMM)
            } else {
                if imm6 == 0 {
                    return 0;
                }
                ((imm6 as u32) << 12 & 0xffff_f000) | (rd << 7) | LUI
            }
        }
        (1, 4) => {
            let shamt = bits(c, 12, 12) << 5 | bits(c, 6, 2);
            match bits(c, 11, 10) {
                0 => i_type(shamt as i32, rs1p, 5, rs1p, OP_IMM),
                1 => i_type((shamt | 0x400) as i32, rs1p, 5, rs1p, OP_IMM),
                2 => i_type(imm6, rs1p, 7, rs1p, OP_IMM),
                _ => {
                    let rs2p = bits(c, 4, 2) + 8;
                    match (bits(c, 12, 12), bits(c, 6, 5)) {
                        (0, 0) => r_type(0x20, rs2p, rs1p, 0, rs1p, OP),
                        (0, 1) => r_type(0, rs2p, rs1p, 4, rs1p, OP),
                        (0, 2) => r_type(0, rs2p, rs1p, 6, rs1p, OP),
                        (0, 3) => r_type(0, rs2p, rs1p, 7, rs1p, OP),
                        (1, 0) => r_type(0x20, rs2p, rs1p, 0, rs1p, OP_32),
                        (1, 1) => r_type(0, rs2p, rs1p, 0, rs1p, OP_32),
                        _ => 0,
                    }
                }
            }
        }
        (1, 5) => {
            let imm = bits(c, 12, 12) << 11 | bits(c, 11, 11) << 4 | bits(c, 10, 9) << 8 | bits(c, 8, 8) << 10
                | bits(c, 7, 7) << 6 | bits(c, 6, 6) << 7 | bits(c, 5, 3) << 1 | bits(c, 2, 2) << 5;
            j_type(sext(imm, 12), 0)
        }
        (1, 6) | (1, 7) => {
            let imm = bits(c, 12, 12) << 8 | bits(c, 11, 10) << 3 | bits(c, 6, 5) << 6 | bits(c, 4, 3) << 1 | bits(c, 2, 2) << 5;
            b_type(sext(imm, 9), 0, rs1p, f3 - 6)
        }
        (2, 0) => {
            let shamt = bits(c, 12, 12) << 5 | bits(c, 6, 2);
            i_type(shamt as i32, rd, 1, rd, OP_IMM)
        }
        (2, 1) => i_type((bits(c, 12, 12) << 5 | bits(c, 6, 5) << 3 | bits(c, 4, 2) << 6) as i32, 2, 3, rd, LOAD_FP),
        (2, 2) => {
            if rd == 0 {
                return 0;
            }
            i_type((bits(c, 12, 12) << 5 | bits(c, 6, 4) << 2 | bits(c, 3, 2) << 6) as i32, 2, 2, rd, LOAD)
        }
        (2, 3) => {
            if rd == 0 {
                return 0;
            }
            i_type((bits(c, 12, 12) << 5 | bits(c, 6, 5) << 3 | bits(c, 4, 2) << 6) as i32, 2, 3, rd, LOAD)
        }
        (2, 4) => match (bits(c, 12, 12), rd, rs2) {
            (0, 0, 0) => 0,
            (0, _, 0) => i_type(0, rd, 0, 0, JALR),
            (0, _, _) => r_type(0, rs2, 0, 0, rd, OP),
            (1, 0, 0) => 0x0010_0073, // ebreak
            (1, _, 0) => i_type(0, rd, 0, 1, JALR),
            (1, _, _) => r_type(0, rs2, rd, 0, rd, OP),
            _ => 0,
        },
        (2, 5) => s_type((bits(c, 12, 10) << 3 | bits(c, 9, 7) << 6) as i32, rs2, 2, 3, STORE_FP),
        (2, 6) => s_type((bits(c, 12, 9) << 2 | bits(c, 8, 7) << 6) as i32, rs2, 2, 2, STORE),
        (2, 7) => s_type((bits(c, 12, 10) << 3 | bits(c, 9, 7) << 6) as i32, rs2, 2, 3, STORE),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::expand;

    #[test]
    fn common_compressed_instructions() {
        // c.addi4spn s0, sp, 16  -> addi s0, sp, 16
        assert_eq!(expand(0x0800), 0x0101_0413);
        // c.li a0, -1 -> addi a0, x0, -1
        assert_eq!(expand(0x557d), 0xfff0_0513);
        // c.addi16sp sp, -64 -> addi sp, sp, -64
        assert_eq!(expand(0x7139), 0xfc01_0113);
        // c.lui a5, 0x1 -> lui a5, 0x1
        assert_eq!(expand(0x6785), 0x0000_17b7);
        // c.ldsp ra, 8(sp) -> ld ra, 8(sp)
        assert_eq!(expand(0x60a2), 0x0081_3083);
        // c.sdsp ra, 8(sp) -> sd ra, 8(sp)
        assert_eq!(expand(0xe406), 0x0011_3423);
        // c.mv a0, a1 -> add a0, x0, a1
        assert_eq!(expand(0x852e), 0x00b0_0533);
        // c.jr ra -> jalr x0, 0(ra)
        assert_eq!(expand(0x8082), 0x0000_8067);
        // c.addw a0, a1 -> addw a0, a0, a1
        assert_eq!(expand(0x9d2d), 0x00b5_053b);
        // c.beqz a0, +8 -> beq a0, x0, 8
        assert_eq!(expand(0xc501), 0x0005_0463);
        // c.j -2 -> jal x0, -2
        assert_eq!(expand(0xbffd), 0xfffff06f);
        // c.ld a5, 0(a0) / c.sd a5, 8(a0)
        assert_eq!(expand(0x611c), 0x0005_3783);
        assert_eq!(expand(0xe51c), 0x00f5_3423);
        // c.srai a0, 63 / c.slli a0, 32
        assert_eq!(expand(0x957d), 0x43f5_5513);
        assert_eq!(expand(0x1502), 0x0205_1513);
        assert_eq!(expand(0), 0);
    }
}
