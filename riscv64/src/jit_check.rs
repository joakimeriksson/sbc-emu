//! A differential test of the block JIT: random programs run in the interpreter and through
//! [`crate::jit::run`], then registers, CSRs, counters and memory are compared. Natively the
//! driver interprets everything (no code generator), which checks the driver; the browser
//! build compiles the programs (`tools/wasm-rv-jit-diff.mjs`).
use crate::jit::{self, Jit};
use crate::{prv, Bus, Hart, Identity};

pub const RAM_BASE: u64 = 0x8000_0000;
const RAM_SIZE: usize = 1 << 20;
const CODE: u64 = RAM_BASE + 0x1000;
const DATA: u64 = RAM_BASE + 0x8_0000;
const PT_ROOT: u64 = RAM_BASE + 0xf_0000;
const DEV: u64 = 0x1000_0000;

/// RAM and one device page whose reads change with every access.
pub struct TestBus {
    pub ram: Vec<u8>,
    pub dev: u64,
}

impl Bus for TestBus {
    fn ram_base(&self) -> u64 {
        RAM_BASE
    }
    fn ram(&mut self) -> &mut [u8] {
        &mut self.ram
    }
    fn read(&mut self, addr: u64, size: u8) -> Result<u64, ()> {
        if addr & !0xfff != DEV {
            return Err(());
        }
        self.dev = self.dev.wrapping_mul(6364136223846793005).wrapping_add(addr);
        Ok(self.dev & if size == 8 { u64::MAX } else { (1 << (8 * size)) - 1 })
    }
    fn write(&mut self, addr: u64, _size: u8, value: u64) -> Result<(), ()> {
        if addr & !0xfff != DEV {
            return Err(());
        }
        self.dev ^= value.rotate_left(addr as u32 & 63);
        Ok(())
    }
    fn mtime(&mut self) -> u64 {
        0
    }
}

pub struct Rng(pub u64);

impl Rng {
    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len() as u64) as usize]
    }
}

/// One instruction of a generated program, before branch offsets are known.
enum Item {
    I32(u32),
    I16(u16),
    /// a branch (B-type without offset) or `jal` (J-type without offset) to `k` items ahead
    Fwd(u32, usize),
    /// a branch back to item `to`
    Back(u32, usize),
    /// `auipc x8, 0; jalr rd, k items ahead(x8)`
    Jalr(u32, usize),
}

/// x5 and x9 hold the data pointer, x6 the device page, x7 a loop counter, x8 a jump base.
const FIXED: [u32; 4] = [5, 6, 7, 8];

fn reg(rng: &mut Rng) -> u32 {
    loop {
        let r = rng.below(32) as u32;
        if !FIXED.contains(&r) && r != 9 {
            return r;
        }
    }
}

fn r_type(f7: u32, f3: u32, op: u32, rd: u32, rs1: u32, rs2: u32) -> u32 {
    f7 << 25 | rs2 << 20 | rs1 << 15 | f3 << 12 | rd << 7 | op
}
fn i_type(imm: i32, f3: u32, op: u32, rd: u32, rs1: u32) -> u32 {
    ((imm as u32) & 0xfff) << 20 | rs1 << 15 | f3 << 12 | rd << 7 | op
}
fn s_type(imm: i32, f3: u32, rs1: u32, rs2: u32) -> u32 {
    let imm = imm as u32 & 0xfff;
    (imm >> 5) << 25 | rs2 << 20 | rs1 << 15 | f3 << 12 | (imm & 31) << 7 | 0x23
}
fn b_offset(i: u32, off: i32) -> u32 {
    let o = off as u32;
    i | ((o >> 12) & 1) << 31 | ((o >> 5) & 0x3f) << 25 | ((o >> 1) & 0xf) << 8 | ((o >> 11) & 1) << 7
}
fn j_offset(i: u32, off: i32) -> u32 {
    let o = off as u32;
    i | ((o >> 20) & 1) << 31 | ((o >> 1) & 0x3ff) << 21 | ((o >> 11) & 1) << 20 | ((o >> 12) & 0xff) << 12
}

/// A random operand value with the corner cases division and shifts care about.
fn value(rng: &mut Rng) -> u64 {
    match rng.below(8) {
        0 => 0,
        1 => u64::MAX,
        2 => 1 << 63,
        3 => 0x8000_0000,
        4 => rng.below(64),
        5 => rng.next_u64() as i32 as i64 as u64,
        _ => rng.next_u64(),
    }
}

fn random_insn(rng: &mut Rng, items: &mut Vec<Item>) {
    let (rd, a, b) = (reg(rng), rng.below(32) as u32, rng.below(32) as u32);
    let item = match rng.below(20) {
        0..=3 => {
            let (f7, f3) = rng.pick(&[
                (0, 0), (0x20, 0), (0, 1), (0, 2), (0, 3), (0, 4), (0, 5), (0x20, 5), (0, 6), (0, 7),
                (1, 0), (1, 1), (1, 2), (1, 3), (1, 4), (1, 5), (1, 6), (1, 7),
                (0x20, 7), (0x20, 6), (0x20, 4), (5, 4), (5, 5), (5, 6), (5, 7), (0x30, 1), (0x30, 5),
                (0x10, 2), (0x10, 4), (0x10, 6),
            ]);
            Item::I32(r_type(f7, f3, 0x33, rd, a, b))
        }
        4..=5 => {
            let (f7, f3) = rng.pick(&[
                (0, 0), (0x20, 0), (0, 1), (0, 5), (0x20, 5), (1, 0), (1, 4), (1, 5), (1, 6), (1, 7),
                (4, 0), (0x10, 2), (0x10, 4), (0x10, 6), (0x30, 1), (0x30, 5),
            ]);
            Item::I32(r_type(f7, f3, 0x3b, rd, a, b))
        }
        6 => Item::I32(r_type(4, 4, 0x3b, rd, a, 0)), // zext.h
        7..=9 => {
            let imm = rng.next_u64() as i32 >> 20;
            let sh = rng.below(64) as i32;
            Item::I32(match rng.below(14) {
                0 => i_type(imm, 0, 0x13, rd, a),
                1 => i_type(imm, 2, 0x13, rd, a),
                2 => i_type(imm, 3, 0x13, rd, a),
                3 => i_type(imm, rng.pick(&[4, 6, 7]), 0x13, rd, a),
                4 => i_type(sh, 1, 0x13, rd, a),
                5 => i_type(sh, 5, 0x13, rd, a),
                6 => i_type(sh | 0x400, 5, 0x13, rd, a),
                7 => i_type(0x600 | rng.pick(&[0, 1, 2, 4, 5]), 1, 0x13, rd, a),
                8 => i_type(0x600 | sh, 5, 0x13, rd, a), // rori
                9 => i_type(rng.pick(&[0x287, 0x6b8]), 5, 0x13, rd, a), // orc.b, rev8
                10 => i_type(imm, 0, 0x1b, rd, a),
                11 => {
                    // slliw, srliw, sraiw, roriw
                    let (f3, hi) = rng.pick(&[(1, 0), (5, 0), (5, 0x400), (5, 0x600)]);
                    i_type(sh & 31 | hi, f3, 0x1b, rd, a)
                }
                12 => i_type(0x80 | sh, 1, 0x1b, rd, a), // slli.uw
                _ => i_type(0x600 | rng.pick(&[0, 1, 2]), 1, 0x1b, rd, a), // clzw, ctzw, cpopw
            })
        }
        10 => Item::I32(if rng.below(2) == 0 { (rng.next_u64() as u32 & 0xffff_f000) | rd << 7 | 0x37 } else { (rng.next_u64() as u32 & 0xffff_f000) | rd << 7 | 0x17 }),
        11..=13 => {
            // loads and stores around the data pointer (some misaligned or straddling a page),
            // a few at the device
            let dev = rng.below(8) == 0;
            let base = if dev { 6 } else { 5 };
            let off = if dev { rng.below(0x800) as i32 & !7 } else { rng.below(4096) as i32 - 2048 };
            let off = if rng.below(4) == 0 { off } else { off & !7 };
            if rng.below(2) == 0 {
                Item::I32(i_type(off, rng.pick(&[0, 1, 2, 3, 4, 5, 6]), 0x03, rd, base))
            } else {
                Item::I32(s_type(off, rng.pick(&[0, 1, 2, 3]), base, b))
            }
        }
        14 => {
            // AMOs at the (aligned) data pointer
            let op = rng.pick(&[0x00, 0x01, 0x04, 0x08, 0x0c, 0x10, 0x14, 0x18, 0x1c]);
            Item::I32(r_type(op << 2, rng.pick(&[2, 3]), 0x2f, rd, 5, b))
        }
        15 => {
            // CSRs: a scratch register and the instruction counter (exact counts)
            let csr = rng.pick(&[0x340, 0xb02, 0x140, 0xc02]);
            Item::I32(i_type(csr, rng.pick(&[1, 2, 3]), 0x73, rd, if csr & 0x800 != 0 { 0 } else { a }))
        }
        16 => Item::I32(0x0ff0_000f), // fence
        17 => {
            // compressed: c.addi, c.li, c.mv, c.add, c.lw, c.sw, c.ld, c.sd (x9 = data pointer)
            let imm = rng.below(64) as u16;
            let rdp = loop {
                let r = rng.below(8) as u16;
                if r != 1 {
                    break r;
                }
            };
            let rd = rd as u16;
            let b = (b as u16).max(1);
            Item::I16(match rng.below(8) {
                0 => (imm >> 5) << 12 | rd << 7 | (imm & 31).max(1) << 2 | 1,
                1 => 0x4000 | (imm >> 5) << 12 | rd << 7 | (imm & 31) << 2 | 1,
                2 => 0x8000 | rd << 7 | b << 2 | 2,
                3 => 0x9000 | rd << 7 | b << 2 | 2,
                4 => 0x4000 | (imm & 7) << 10 | 1 << 7 | (imm >> 3 & 1) << 6 | (imm >> 4 & 1) << 5 | rdp << 2,
                5 => 0xc000 | (imm & 7) << 10 | 1 << 7 | (imm >> 3 & 1) << 6 | (imm >> 4 & 1) << 5 | rdp << 2,
                6 => 0x6000 | (imm & 7) << 10 | 1 << 7 | (imm >> 3 & 3) << 5 | rdp << 2,
                _ => 0xe000 | (imm & 7) << 10 | 1 << 7 | (imm >> 3 & 3) << 5 | rdp << 2,
            })
        }
        18 => {
            let k = 1 + rng.below(4) as usize;
            if rng.below(3) == 0 {
                Item::Fwd(rd << 7 | 0x6f, k)
            } else {
                Item::Fwd(r_type(0, rng.pick(&[0, 1, 4, 5, 6, 7]), 0x63, 0, a, b), k)
            }
        }
        _ => Item::Jalr(i_type(0, 0, 0x67, rd, 8), 1 + rng.below(3) as usize),
    };
    items.push(item);
}

/// A random program ending in `wfi`; `loops`: its middle runs in a counted loop.
pub fn random_program(rng: &mut Rng, loops: bool) -> Vec<u8> {
    let mut items = Vec::new();
    let n = 8 + rng.below(40) as usize;
    let loop_start = if loops {
        items.push(Item::I32(i_type(2 + rng.below(20) as i32, 0, 0x13, 7, 0))); // li x7, N
        Some(items.len())
    } else {
        None
    };
    for _ in 0..n {
        random_insn(rng, &mut items);
    }
    // forward jumps stay before the loop's decrement (or reach the wfi without a loop)
    let fwd_limit = items.len();
    if let Some(start) = loop_start {
        items.push(Item::I32(i_type(-1, 0, 0x13, 7, 7))); // addi x7, x7, -1
        items.push(Item::Back(r_type(0, 1, 0x63, 0, 7, 0), start)); // bne x7, x0, start
    }
    // the wfi is the trap vector: 4-byte aligned (mtvec's low bits are its mode)
    let len: usize = items.iter().map(|it| match it {
        Item::I16(_) => 2,
        Item::Jalr(..) => 8,
        _ => 4,
    }).sum();
    if !len.is_multiple_of(4) {
        items.push(Item::I16(0x0001)); // c.nop
    }
    items.push(Item::I32(0x1050_0073)); // wfi
    // lay out, then fill in the offsets (a forward target past the end is the wfi)
    let mut at = Vec::with_capacity(items.len() + 1);
    let mut pos = 0u32;
    for it in &items {
        at.push(pos);
        pos += match it {
            Item::I16(_) => 2,
            Item::Jalr(..) => 8,
            _ => 4,
        };
    }
    let last = if loops { fwd_limit } else { items.len() - 1 };
    let mut out = Vec::new();
    for (k, it) in items.iter().enumerate() {
        let here = at[k] as i32;
        match *it {
            Item::I32(i) => out.extend_from_slice(&i.to_le_bytes()),
            Item::I16(h) => out.extend_from_slice(&h.to_le_bytes()),
            Item::Fwd(i, d) => {
                let to = at[(k + d).min(last)] as i32 - here;
                let w = if i & 0x7f == 0x6f { j_offset(i, to) } else { b_offset(i, to) };
                out.extend_from_slice(&w.to_le_bytes());
            }
            Item::Back(i, to) => out.extend_from_slice(&b_offset(i, at[to] as i32 - here).to_le_bytes()),
            Item::Jalr(i, d) => {
                let to = at[(k + d).min(last)] as i32 - here;
                out.extend_from_slice(&(8 << 7 | 0x17u32).to_le_bytes()); // auipc x8, 0
                out.extend_from_slice(&(i | ((to as u32) & 0xfff) << 20).to_le_bytes());
            }
        }
    }
    out
}

/// A hart and memory with `code` at CODE and registers and data from `seed`; in S mode with
/// Sv39 (identity gigapages) when `smode`.
fn setup(code: &[u8], seed: u64, smode: bool) -> (Hart, TestBus) {
    let mut rng = Rng(seed | 1);
    let mut bus = TestBus { ram: vec![0; RAM_SIZE], dev: seed };
    for b in bus.ram.iter_mut() {
        *b = rng.next_u64() as u8;
    }
    let c = (CODE - RAM_BASE) as usize;
    bus.ram[c..c + code.len()].copy_from_slice(code);
    let mut h = Hart::new(Identity::sifive_u74(0));
    for r in 1..32 {
        h.x[r] = value(&mut rng);
    }
    h.x[5] = DATA + (rng.below(256) & !7);
    h.x[9] = h.x[5];
    h.x[6] = DEV;
    h.pc = CODE;
    // a trap ends the run at the final wfi
    let wfi = CODE + code.len() as u64 - 4;
    h.mtvec = wfi;
    h.stvec = wfi;
    h.mcounteren = u64::MAX;
    h.scounteren = u64::MAX;
    if smode {
        let root = (PT_ROOT - RAM_BASE) as usize;
        let pte = |pa: u64, flags: u64| (pa >> 12) << 10 | flags;
        // V R W X A D, and the device gigapage R W
        bus.ram[root + 16..root + 24].copy_from_slice(&pte(RAM_BASE, 0xcf).to_le_bytes());
        bus.ram[root..root + 8].copy_from_slice(&pte(0, 0xc7).to_le_bytes());
        for k in [1, 3, 4, 5, 6, 7] {
            bus.ram[root + 8 * k..root + 8 * k + 8].fill(0);
        }
        h.satp = 8 << 60 | PT_ROOT >> 12;
        h.prv = prv::S;
        h.medeleg = u64::MAX;
    }
    (h, bus)
}

/// Run `code` from `seed` in the interpreter and through the JIT (compiling an entry after
/// `hot` executions) and compare.
pub fn compare(code: &[u8], seed: u64, smode: bool, hot: u32) -> Result<(), String> {
    let (mut a, mut abus) = setup(code, seed, smode);
    let (mut b, mut bbus) = setup(code, seed, smode);
    a.run(&mut abus, 1_000_000);
    let mut j = Jit::new();
    j.hot = hot;
    let mut rounds = 0;
    while !b.wfi && rounds < 1000 {
        jit::run(&mut b, &mut bbus, &mut j, 1000);
        rounds += 1;
    }
    let mut diffs = Vec::new();
    for r in 0..32 {
        if a.x[r] != b.x[r] {
            diffs.push(format!("x{r}: {:#x} (interpreter) {:#x} (JIT)", a.x[r], b.x[r]));
        }
    }
    for (name, x, y) in [
        ("pc", a.pc, b.pc),
        ("instret", a.instret, b.instret),
        ("cycle", a.cycle, b.cycle),
        ("mscratch", a.mscratch, b.mscratch),
        ("sscratch", a.sscratch, b.sscratch),
        ("mcause", a.mcause, b.mcause),
        ("scause", a.scause, b.scause),
        ("device", abus.dev, bbus.dev),
        ("wfi", a.wfi as u64, b.wfi as u64),
    ] {
        if x != y {
            diffs.push(format!("{name}: {x:#x} (interpreter) {y:#x} (JIT)"));
        }
    }
    if let Some(o) = abus.ram.iter().zip(&bbus.ram).position(|(x, y)| x != y) {
        diffs.push(format!("memory differs first at {:#x}", RAM_BASE + o as u64));
    }
    if diffs.is_empty() {
        Ok(())
    } else {
        Err(format!("{} (compiled {})", diffs.join("; "), j.compiled))
    }
}

/// Run `cases` random programs from `seed`; the first failure's description.
pub fn run_cases(cases: u32, seed: u64, loops: bool, smode: bool, hot: u32) -> (u32, Option<String>) {
    let mut rng = Rng(seed | 1);
    let mut failures = 0;
    let mut first = None;
    for case in 0..cases {
        let code = random_program(&mut rng, loops);
        let s = rng.next_u64();
        if let Err(e) = compare(&code, s, smode, hot) {
            if first.is_none() {
                let words: Vec<String> = code.chunks(2).map(|c| format!("{:04x}", u16::from_le_bytes([c[0], c[1]]))).collect();
                first = Some(format!("case {case} (seed {s:#x}, smode {smode}): {e}\ncode (halfwords) {}", words.join(" ")));
            }
            failures += 1;
        }
    }
    (failures, first)
}

#[cfg(test)]
mod tests {
    #[test]
    fn driver_matches_the_interpreter() {
        for (loops, smode) in [(false, false), (true, false), (false, true), (true, true)] {
            let (fails, first) = super::run_cases(300, 0x5eed + loops as u64 * 7 + smode as u64 * 13, loops, smode, 1);
            assert_eq!(fails, 0, "{}", first.unwrap_or_default());
        }
    }
}
