//! A tiny x86-64 encoder: only the instructions the block compilers emit, in the forms an
//! assembler picks for them (the short `imm8` and `eax` forms, `disp8` where it fits), so that
//! every encoding can be checked against clang's assembler: `tests::encodings_match_fixture`
//! compares with the fixture `tests::external_encodings_match_clang` regenerates. Pure byte
//! output, so it builds (and its fixture test runs) on every host; only x86-64 runs the code.
#![allow(dead_code)]

pub type Reg = u8;
pub const RAX: Reg = 0;
pub const RCX: Reg = 1;
pub const RDX: Reg = 2;
pub const RBX: Reg = 3;
pub const RSP: Reg = 4;
pub const RBP: Reg = 5;
pub const RSI: Reg = 6;
pub const RDI: Reg = 7;
pub const R8: Reg = 8;
pub const R9: Reg = 9;
pub const R10: Reg = 10;
pub const R11: Reg = 11;
pub const R12: Reg = 12;
pub const R13: Reg = 13;
pub const R14: Reg = 14;
pub const R15: Reg = 15;

/// Condition codes (the low nibble of `jcc`, `setcc`, `cmovcc`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cond { O = 0, No = 1, B = 2, Ae = 3, E = 4, Ne = 5, Be = 6, A = 7, S = 8, Ns = 9, P = 10, Np = 11, L = 12, Ge = 13, Le = 14, G = 15 }
impl Cond {
    pub fn invert(self) -> Cond {
        use Cond::*;
        [No, O, Ae, B, Ne, E, A, Be, Ns, S, Np, P, Ge, L, G, Le][self as usize]
    }
}

/// The group-1 ALU operations (the `/n` of `81`/`83`, and `n * 8 + 1`/`+ 3` for the register forms).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Alu { Add = 0, Or = 1, Adc = 2, Sbb = 3, And = 4, Sub = 5, Xor = 6, Cmp = 7 }

/// The group-2 shifts (the `/n` of `C1`/`D1`/`D3`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shift { Rol = 0, Ror = 1, Shl = 4, Shr = 5, Sar = 7 }

#[derive(Clone, Copy)]
pub struct Label(pub usize);

pub struct Asm {
    pub code: Vec<u8>,
    labels: Vec<Option<usize>>,
    /// (offset of a rel32 field, label): the field is relative to its own end
    fixups: Vec<(usize, usize)>,
}

#[allow(clippy::new_without_default, reason = "assembler construction reserves its expected code capacity")]
impl Asm {
    pub fn new() -> Self { Asm { code: Vec::with_capacity(1024), labels: Vec::new(), fixups: Vec::new() } }
    #[allow(clippy::len_without_is_empty, reason = "the length is exposed as a code offset, not as a collection API")]
    pub fn len(&self) -> usize { self.code.len() }
    pub fn here(&self) -> usize { self.code.len() }
    pub fn byte(&mut self, b: u8) { self.code.push(b); }
    pub fn bytes(&mut self, b: &[u8]) { self.code.extend_from_slice(b); }
    fn d32(&mut self, v: u32) { self.code.extend_from_slice(&v.to_le_bytes()); }

    pub fn label(&mut self) -> Label { self.labels.push(None); Label(self.labels.len() - 1) }
    pub fn bind(&mut self, l: Label) { self.labels[l.0] = Some(self.code.len()); }
    pub fn is_bound(&self, l: Label) -> bool { self.labels[l.0].is_some() }

    /// Resolve every jump to its label; every label must be bound.
    pub fn finish(mut self) -> Vec<u8> {
        for (at, l) in std::mem::take(&mut self.fixups) {
            let target = self.labels[l].expect("unbound label");
            let off = target as i64 - (at as i64 + 4);
            let off = i32::try_from(off).expect("jump out of rel32 range");
            self.code[at..at + 4].copy_from_slice(&off.to_le_bytes());
        }
        self.code
    }

    // ---------------------------------------------------------------- prefixes and operands
    /// REX when needed: W, the high bits of reg/index/base, or `byte` for spl..dil as the
    /// ModRM `rm` (or opcode) byte register.
    fn rex(&mut self, w: bool, r: Reg, x: Reg, b: Reg, byte: bool) {
        let v = 0x40 | (w as u8) << 3 | (r >> 3) << 2 | (x >> 3) << 1 | (b >> 3);
        if v != 0x40 || (byte && (4..8).contains(&b)) { self.byte(v); }
    }
    fn modrm_rr(&mut self, reg: Reg, rm: Reg) { self.byte(0xc0 | (reg & 7) << 3 | (rm & 7)); }
    /// `[base + disp]`: the shortest form, with the SIB rsp and r12 need and the disp rbp and r13 need.
    fn modrm_mem(&mut self, reg: Reg, base: Reg, disp: i32) {
        let md: u8 = if disp == 0 && base & 7 != 5 { 0 } else if (-128..128).contains(&disp) { 1 } else { 2 };
        self.byte(md << 6 | (reg & 7) << 3 | (base & 7));
        if base & 7 == 4 { self.byte(0x24); }
        match md { 1 => self.byte(disp as i8 as u8), 2 => self.d32(disp as u32), _ => {} }
    }
    /// opcode bytes with a register-register ModRM
    fn op_rr(&mut self, w: bool, op: &[u8], reg: Reg, rm: Reg) { self.rex(w, reg, 0, rm, false); self.bytes(op); self.modrm_rr(reg, rm); }
    /// opcode bytes with a memory ModRM
    fn op_rm(&mut self, w: bool, op: &[u8], reg: Reg, base: Reg, disp: i32) { self.rex(w, reg, 0, base, false); self.bytes(op); self.modrm_mem(reg, base, disp); }

    // ---------------------------------------------------------------- moves
    /// `mov r32, imm32` (zero-extends)
    pub fn mov_ri(&mut self, d: Reg, imm: u32) { self.rex(false, 0, 0, d, false); self.byte(0xb8 + (d & 7)); self.d32(imm); }
    /// `movabs r64, imm64`
    pub fn mov_ri64(&mut self, d: Reg, imm: u64) { self.rex(true, 0, 0, d, false); self.byte(0xb8 + (d & 7)); self.bytes(&imm.to_le_bytes()); }
    /// `mov r32, r32` (zero-extends)
    pub fn mov_rr(&mut self, d: Reg, s: Reg) { self.op_rr(false, &[0x89], s, d); }
    pub fn mov_rr64(&mut self, d: Reg, s: Reg) { self.op_rr(true, &[0x89], s, d); }
    /// `mov r32, dword [base + disp]`
    pub fn load32(&mut self, d: Reg, base: Reg, disp: i32) { self.op_rm(false, &[0x8b], d, base, disp); }
    pub fn load64(&mut self, d: Reg, base: Reg, disp: i32) { self.op_rm(true, &[0x8b], d, base, disp); }
    /// `mov dword [base + disp], r32`
    pub fn store32(&mut self, base: Reg, disp: i32, s: Reg) { self.op_rm(false, &[0x89], s, base, disp); }
    pub fn store64(&mut self, base: Reg, disp: i32, s: Reg) { self.op_rm(true, &[0x89], s, base, disp); }
    /// `mov dword [base + disp], imm32`
    pub fn store32_imm(&mut self, base: Reg, disp: i32, imm: u32) { self.op_rm(false, &[0xc7], 0, base, disp); self.d32(imm); }
    /// `lea r32, [base + disp]`
    pub fn lea32(&mut self, d: Reg, base: Reg, disp: i32) { self.op_rm(false, &[0x8d], d, base, disp); }
    pub fn movsx8(&mut self, d: Reg, s: Reg) { self.rex(false, d, 0, s, true); self.bytes(&[0x0f, 0xbe]); self.modrm_rr(d, s); }
    pub fn movsx16(&mut self, d: Reg, s: Reg) { self.op_rr(false, &[0x0f, 0xbf], d, s); }
    pub fn movzx8(&mut self, d: Reg, s: Reg) { self.rex(false, d, 0, s, true); self.bytes(&[0x0f, 0xb6]); self.modrm_rr(d, s); }
    pub fn movzx16(&mut self, d: Reg, s: Reg) { self.op_rr(false, &[0x0f, 0xb7], d, s); }
    /// `movsxd r64, r32`
    pub fn movsxd(&mut self, d: Reg, s: Reg) { self.op_rr(true, &[0x63], d, s); }
    pub fn push(&mut self, r: Reg) { self.rex(false, 0, 0, r, false); self.byte(0x50 + (r & 7)); }
    pub fn pop(&mut self, r: Reg) { self.rex(false, 0, 0, r, false); self.byte(0x58 + (r & 7)); }

    // ---------------------------------------------------------------- arithmetic
    /// `op r32, r32` (`w`: 64-bit)
    pub fn alu_rr(&mut self, op: Alu, w: bool, d: Reg, s: Reg) { self.op_rr(w, &[(op as u8) << 3 | 1], s, d); }
    /// `op r32, dword [base + disp]`
    pub fn alu_rm(&mut self, op: Alu, d: Reg, base: Reg, disp: i32) { self.op_rm(false, &[(op as u8) << 3 | 3], d, base, disp); }
    /// `op r32, imm` (`w`: 64-bit, the immediate sign-extended)
    pub fn alu_ri(&mut self, op: Alu, w: bool, d: Reg, imm: i32) {
        if (-128..128).contains(&imm) { self.op_rr(w, &[0x83], op as u8, d); self.byte(imm as i8 as u8); }
        else if d == RAX { self.rex(w, 0, 0, 0, false); self.byte((op as u8) << 3 | 5); self.d32(imm as u32); }
        else { self.op_rr(w, &[0x81], op as u8, d); self.d32(imm as u32); }
    }
    /// `op dword/qword [base + disp], imm`
    pub fn alu_mi(&mut self, op: Alu, w: bool, base: Reg, disp: i32, imm: i32) {
        if (-128..128).contains(&imm) { self.op_rm(w, &[0x83], op as u8, base, disp); self.byte(imm as i8 as u8); }
        else { self.op_rm(w, &[0x81], op as u8, base, disp); self.d32(imm as u32); }
    }
    /// `test r32, r32`
    pub fn test_rr(&mut self, w: bool, a: Reg, b: Reg) { self.op_rr(w, &[0x85], b, a); }
    /// `shl/shr/sar/rol/ror r32, imm8` (`w`: 64-bit)
    pub fn shift_ri(&mut self, op: Shift, w: bool, d: Reg, imm: u8) {
        if imm == 1 { self.op_rr(w, &[0xd1], op as u8, d); } else { self.op_rr(w, &[0xc1], op as u8, d); self.byte(imm); }
    }
    /// `shl/shr/sar/rol/ror r32, cl`
    pub fn shift_cl(&mut self, op: Shift, d: Reg) { self.op_rr(false, &[0xd3], op as u8, d); }
    /// `imul r32, r32` (`w`: 64-bit)
    pub fn imul_rr(&mut self, w: bool, d: Reg, s: Reg) { self.op_rr(w, &[0x0f, 0xaf], d, s); }
    pub fn not(&mut self, d: Reg) { self.op_rr(false, &[0xf7], 2, d); }
    pub fn neg(&mut self, d: Reg) { self.op_rr(false, &[0xf7], 3, d); }
    /// `div r32`: edx:eax / r, quotient in eax, remainder in edx
    pub fn div(&mut self, s: Reg) { self.op_rr(false, &[0xf7], 6, s); }
    pub fn idiv(&mut self, s: Reg) { self.op_rr(false, &[0xf7], 7, s); }
    /// `cdq`: sign-extend eax into edx
    pub fn cdq(&mut self) { self.byte(0x99); }
    pub fn bsr(&mut self, d: Reg, s: Reg) { self.op_rr(false, &[0x0f, 0xbd], d, s); }
    pub fn bsf(&mut self, d: Reg, s: Reg) { self.op_rr(false, &[0x0f, 0xbc], d, s); }
    pub fn bswap(&mut self, d: Reg) { self.rex(false, 0, 0, d, false); self.bytes(&[0x0f, 0xc8 + (d & 7)]); }
    /// `setcc r8`
    pub fn setcc(&mut self, c: Cond, d: Reg) { self.rex(false, 0, 0, d, true); self.bytes(&[0x0f, 0x90 + c as u8]); self.modrm_rr(0, d); }
    /// `cmovcc r32, r32`
    pub fn cmov(&mut self, c: Cond, d: Reg, s: Reg) { self.op_rr(false, &[0x0f, 0x40 + c as u8], d, s); }

    // ---------------------------------------------------------------- control
    /// `call r64`
    pub fn call_r(&mut self, r: Reg) { self.rex(false, 0, 0, r, false); self.byte(0xff); self.modrm_rr(2, r); }
    pub fn ret(&mut self) { self.byte(0xc3); }
    /// `jmp rel32`
    pub fn jmp(&mut self, l: Label) { self.byte(0xe9); self.fixups.push((self.code.len(), l.0)); self.d32(0); }
    /// `jcc rel32`
    pub fn jcc(&mut self, c: Cond, l: Label) { self.bytes(&[0x0f, 0x80 + c as u8]); self.fixups.push((self.code.len(), l.0)); self.d32(0); }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every instruction form the compilers use, as Intel-syntax assembler text and as our
    /// bytes, with each line's byte range.
    fn program() -> (Vec<String>, Vec<u8>, Vec<(usize, usize)>) {
        let mut a = Asm::new();
        let mut text = Vec::new();
        let mut ranges = Vec::new();
        let l_end = a.label();
        macro_rules! t { ($s:expr, $e:expr) => {{ let at = a.here(); $e; ranges.push((at, a.here())); text.push($s.to_string()); }}; }
        t!("mov eax, 0x12345678", a.mov_ri(RAX, 0x1234_5678));
        t!("mov r13d, 0", a.mov_ri(R13, 0));
        t!("movabs rcx, 0x123456789abcdef0", a.mov_ri64(RCX, 0x1234_5678_9abc_def0));
        t!("movabs r9, 0x10", a.mov_ri64(R9, 0x10));
        t!("mov ecx, eax", a.mov_rr(RCX, RAX));
        t!("mov r14d, ecx", a.mov_rr(R14, RCX));
        t!("mov rbx, rdi", a.mov_rr64(RBX, RDI));
        t!("mov r12, rdx", a.mov_rr64(R12, RDX));
        t!("mov rdi, rbx", a.mov_rr64(RDI, RBX));
        t!("mov eax, dword ptr [rbx + 0x40]", a.load32(RAX, RBX, 0x40));
        t!("mov ecx, dword ptr [rbx + 0x400]", a.load32(RCX, RBX, 0x400));
        t!("mov eax, dword ptr [r15 + 0x1234]", a.load32(RAX, R15, 0x1234));
        t!("mov eax, dword ptr [r12]", a.load32(RAX, R12, 0));
        t!("mov eax, dword ptr [r13]", a.load32(RAX, R13, 0));
        t!("mov eax, dword ptr [rbp]", a.load32(RAX, RBP, 0));
        t!("mov eax, dword ptr [rsp + 8]", a.load32(RAX, RSP, 8));
        t!("mov ecx, dword ptr [rax - 4]", a.load32(RCX, RAX, -4));
        t!("mov r8d, dword ptr [rbx + 0x8c]", a.load32(R8, RBX, 0x8c));
        t!("mov rax, qword ptr [rsp + 8]", a.load64(RAX, RSP, 8));
        t!("mov r15, qword ptr [r12 + 8]", a.load64(R15, R12, 8));
        t!("mov dword ptr [rbx + 0x44], eax", a.store32(RBX, 0x44, RAX));
        t!("mov dword ptr [rbx + 0x200], edx", a.store32(RBX, 0x200, RDX));
        t!("mov dword ptr [r12 + 4], r9d", a.store32(R12, 4, R9));
        t!("mov qword ptr [rsp + 8], rax", a.store64(RSP, 8, RAX));
        t!("mov dword ptr [rbx + 0x80], 0x40380200", a.store32_imm(RBX, 0x80, 0x4038_0200));
        t!("mov dword ptr [r12], 0x40380200", a.store32_imm(R12, 0, 0x4038_0200));
        t!("mov dword ptr [rsp], 2", a.store32_imm(RSP, 0, 2));
        t!("lea eax, [r13 + 5]", a.lea32(RAX, R13, 5));
        t!("lea eax, [r13 + 0x200]", a.lea32(RAX, R13, 0x200));
        t!("movsx eax, al", a.movsx8(RAX, RAX));
        t!("movsx eax, sil", a.movsx8(RAX, RSI));
        t!("movsx eax, ax", a.movsx16(RAX, RAX));
        t!("movzx eax, ax", a.movzx16(RAX, RAX));
        t!("movzx eax, dl", a.movzx8(RAX, RDX));
        t!("movzx ecx, dil", a.movzx8(RCX, RDI));
        t!("movsxd rax, eax", a.movsxd(RAX, RAX));
        t!("movsxd rcx, ecx", a.movsxd(RCX, RCX));
        t!("push rbx", a.push(RBX));
        t!("push r15", a.push(R15));
        t!("pop r12", a.pop(R12));
        t!("pop rbp", a.pop(RBP));
        t!("add eax, ecx", a.alu_rr(Alu::Add, false, RAX, RCX));
        t!("sub eax, ecx", a.alu_rr(Alu::Sub, false, RAX, RCX));
        t!("and eax, ecx", a.alu_rr(Alu::And, false, RAX, RCX));
        t!("or eax, ecx", a.alu_rr(Alu::Or, false, RAX, RCX));
        t!("xor eax, ecx", a.alu_rr(Alu::Xor, false, RAX, RCX));
        t!("cmp eax, ecx", a.alu_rr(Alu::Cmp, false, RAX, RCX));
        t!("xor r13d, r13d", a.alu_rr(Alu::Xor, false, R13, R13));
        t!("or rax, rdx", a.alu_rr(Alu::Or, true, RAX, RDX));
        t!("add eax, dword ptr [rbx + 0x48]", a.alu_rm(Alu::Add, RAX, RBX, 0x48));
        t!("cmp eax, dword ptr [rbx + 0x7c]", a.alu_rm(Alu::Cmp, RAX, RBX, 0x7c));
        t!("cmp ecx, dword ptr [r12 + 4]", a.alu_rm(Alu::Cmp, RCX, R12, 4));
        t!("xor eax, dword ptr [rbx + 0x84]", a.alu_rm(Alu::Xor, RAX, RBX, 0x84));
        t!("add eax, 5", a.alu_ri(Alu::Add, false, RAX, 5));
        t!("add eax, -2048", a.alu_ri(Alu::Add, false, RAX, -2048));
        t!("add ecx, 0x7ff", a.alu_ri(Alu::Add, false, RCX, 0x7ff));
        t!("and ecx, -2", a.alu_ri(Alu::And, false, RCX, -2));
        t!("and eax, 0xffff", a.alu_ri(Alu::And, false, RAX, 0xffff));
        t!("cmp edx, 1", a.alu_ri(Alu::Cmp, false, RDX, 1));
        t!("cmp r14d, 24", a.alu_ri(Alu::Cmp, false, R14, 24));
        t!("sub r14d, 200", a.alu_ri(Alu::Sub, false, R14, 200));
        t!("add r13d, 3", a.alu_ri(Alu::Add, false, R13, 3));
        t!("sub rsp, 24", a.alu_ri(Alu::Sub, true, RSP, 24));
        t!("add rsp, 24", a.alu_ri(Alu::Add, true, RSP, 24));
        t!("add qword ptr [rbx + 0x100], 7", a.alu_mi(Alu::Add, true, RBX, 0x100, 7));
        t!("add qword ptr [rbx + 0x108], 300", a.alu_mi(Alu::Add, true, RBX, 0x108, 300));
        t!("cmp dword ptr [r15 + 0x40], 3", a.alu_mi(Alu::Cmp, false, R15, 0x40, 3));
        t!("cmp dword ptr [r15 + 0x40], 0x12345", a.alu_mi(Alu::Cmp, false, R15, 0x40, 0x12345));
        t!("cmp dword ptr [rax], 0x40381", a.alu_mi(Alu::Cmp, false, RAX, 0, 0x40381));
        t!("cmp qword ptr [rsp + 8], 0", a.alu_mi(Alu::Cmp, true, RSP, 8, 0));
        t!("test eax, eax", a.test_rr(false, RAX, RAX));
        t!("test ecx, ecx", a.test_rr(false, RCX, RCX));
        t!("test rax, rax", a.test_rr(true, RAX, RAX));
        t!("shl eax, 5", a.shift_ri(Shift::Shl, false, RAX, 5));
        t!("shl eax, 1", a.shift_ri(Shift::Shl, false, RAX, 1));
        t!("shr eax, 31", a.shift_ri(Shift::Shr, false, RAX, 31));
        t!("sar eax, 7", a.shift_ri(Shift::Sar, false, RAX, 7));
        t!("ror eax, 3", a.shift_ri(Shift::Ror, false, RAX, 3));
        t!("shr rax, 32", a.shift_ri(Shift::Shr, true, RAX, 32));
        t!("shr rdx, 32", a.shift_ri(Shift::Shr, true, RDX, 32));
        t!("shl rdx, 32", a.shift_ri(Shift::Shl, true, RDX, 32));
        t!("shl eax, cl", a.shift_cl(Shift::Shl, RAX));
        t!("shr eax, cl", a.shift_cl(Shift::Shr, RAX));
        t!("sar eax, cl", a.shift_cl(Shift::Sar, RAX));
        t!("rol eax, cl", a.shift_cl(Shift::Rol, RAX));
        t!("ror eax, cl", a.shift_cl(Shift::Ror, RAX));
        t!("shl edx, cl", a.shift_cl(Shift::Shl, RDX));
        t!("imul eax, ecx", a.imul_rr(false, RAX, RCX));
        t!("imul rax, rcx", a.imul_rr(true, RAX, RCX));
        t!("not ecx", a.not(RCX));
        t!("not eax", a.not(RAX));
        t!("neg eax", a.neg(RAX));
        t!("div ecx", a.div(RCX));
        t!("idiv ecx", a.idiv(RCX));
        t!("cdq", a.cdq());
        t!("bsr eax, ecx", a.bsr(RAX, RCX));
        t!("bsf eax, ecx", a.bsf(RAX, RCX));
        t!("bswap eax", a.bswap(RAX));
        t!("bswap r9d", a.bswap(R9));
        t!("setl dl", a.setcc(Cond::L, RDX));
        t!("setb dl", a.setcc(Cond::B, RDX));
        t!("sete al", a.setcc(Cond::E, RAX));
        t!("setne sil", a.setcc(Cond::Ne, RSI));
        t!("setg r8b", a.setcc(Cond::G, R8));
        t!("cmovl eax, ecx", a.cmov(Cond::L, RAX, RCX));
        t!("cmovb eax, ecx", a.cmov(Cond::B, RAX, RCX));
        t!("cmovg eax, ecx", a.cmov(Cond::G, RAX, RCX));
        t!("cmova eax, ecx", a.cmov(Cond::A, RAX, RCX));
        t!("cmove eax, edx", a.cmov(Cond::E, RAX, RDX));
        t!("call rax", a.call_r(RAX));
        t!("call r11", a.call_r(R11));
        t!("ret", a.ret());
        t!("jmp 1f", a.jmp(l_end));
        t!("je 1f", a.jcc(Cond::E, l_end));
        t!("jne 1f", a.jcc(Cond::Ne, l_end));
        t!("jb 1f", a.jcc(Cond::B, l_end));
        t!("jae 1f", a.jcc(Cond::Ae, l_end));
        t!("jl 1f", a.jcc(Cond::L, l_end));
        t!("jge 1f", a.jcc(Cond::Ge, l_end));
        // far enough that the assembler keeps the rel32 forms
        t!(".fill 200, 1, 0x90", a.bytes(&[0x90; 200]));
        text.push("1:".to_string());
        a.bind(l_end);
        (text, a.finish(), ranges)
    }

    fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{:02x}", x)).collect() }

    /// Hermetic: every instruction must match `tests/fixtures/x64_expected.hex`, which
    /// `external_encodings_match_clang` produced from clang's assembler (run it with
    /// `UPDATE_FIXTURE=1` after adding an instruction).
    #[test]
    fn encodings_match_fixture() {
        let (text, ours, ranges) = program();
        let text: Vec<&String> = text.iter().filter(|l| !l.ends_with(':')).collect();
        let want: Vec<&str> = include_str!("../tests/fixtures/x64_expected.hex").lines().filter(|l| !l.starts_with('#') && !l.is_empty()).collect();
        assert_eq!(want.len(), ranges.len(), "instruction count differs from the fixture; regenerate it with clang");
        for (i, ((w, &(s, e)), line)) in want.iter().zip(&ranges).zip(&text).enumerate() {
            assert_eq!(w.trim(), hex(&ours[s..e]), "#{} (fixture) != (ours): {}", i, line);
        }
    }

    /// Assemble the reference text with clang and compare byte for byte with our encoder.
    /// `CLANG` and `LLVM_OBJCOPY` name the tools (default: `clang`, `llvm-objcopy`).
    #[test]
    #[ignore = "needs clang and llvm-objcopy; regenerates the fixture with UPDATE_FIXTURE=1"]
    fn external_encodings_match_clang() {
        let clang = std::env::var("CLANG").unwrap_or_else(|_| "clang".into());
        let objcopy = std::env::var("LLVM_OBJCOPY").unwrap_or_else(|_| "llvm-objcopy".into());
        let (text, ours, ranges) = program();
        let dir = std::env::temp_dir().join(format!("x64enc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("t.s"), format!(".intel_syntax noprefix\n.text\n{}\n", text.join("\n"))).unwrap();
        let st = std::process::Command::new(&clang).args(["-c", "--target=x86_64-unknown-linux-gnu", "-x", "assembler", "-o"]).arg(dir.join("t.o")).arg(dir.join("t.s")).status().unwrap();
        assert!(st.success(), "clang failed");
        let st = std::process::Command::new(&objcopy).args(["-O", "binary", "--only-section=.text"]).arg(dir.join("t.o")).arg(dir.join("t.bin")).status().unwrap();
        assert!(st.success(), "llvm-objcopy failed");
        let theirs = std::fs::read(dir.join("t.bin")).unwrap();
        let text: Vec<&String> = text.iter().filter(|l| !l.ends_with(':')).collect();
        for (&(s, e), line) in ranges.iter().zip(&text) {
            assert!(e <= theirs.len(), "clang's code is shorter: {}", line);
            assert_eq!(hex(&theirs[s..e]), hex(&ours[s..e]), "(clang) != (ours): {}", line);
        }
        assert_eq!(theirs.len(), ours.len(), "code length differs");
        let _ = std::fs::remove_dir_all(&dir);
        if std::env::var("UPDATE_FIXTURE").is_ok() {
            let mut f = String::from("# x86-64 encodings of the reference program in jit_x64.rs tests, as clang assembled them, one instruction per line.\n");
            for &(s, e) in &ranges { f += &hex(&theirs[s..e]); f.push('\n'); }
            std::fs::write(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/x64_expected.hex"), f).unwrap();
        }
    }
}
