//! A decoded program: the instructions reachable from an entry point.

use crate::decode::{decode, past_sched};
use crate::isa::*;
use std::collections::BTreeMap;

/// Reachable code, decoded and linked. Jump targets are indices into `code`.
#[derive(Clone, Debug)]
pub struct Program {
    pub(crate) code: Vec<Inst>,
    /// Code address and original word of every instruction, in address order.
    pub(crate) words: Vec<(u32, u64)>,
    /// SM70: the upper 64-bit half of every instruction word (address, word), which `words` lacks.
    pub(crate) high: Vec<(u32, u64)>,
    pub(crate) entry: u32,
    gprs: u32,
    /// The JIT's code, made when first wanted.
    jit: std::sync::OnceLock<Option<std::sync::Arc<crate::jit::Code>>>,
}

/// Upper bound on the instructions of one program (a runaway decode of garbage).
const MAX_INSTS: usize = 1 << 18;

impl Program {
    /// Decodes everything reachable from code address `entry` (the first instruction, after
    /// the program header). `fetch` returns the 64-bit word at a code address, or None outside
    /// the code segment. Every fourth word (address a multiple of 32) is scheduling control
    /// and is skipped. Functions called by absolute address (the driver's built-in division
    /// and f64 reciprocal routines) are decoded too.
    pub fn decode_at(
        entry: u32,
        fetch: &mut dyn FnMut(u32) -> Option<u64>,
    ) -> Result<Program, String> {
        let entry = past_sched(entry);
        let mut found: BTreeMap<u32, (u64, Inst)> = BTreeMap::new();
        let mut work = vec![entry];
        while let Some(mut addr) = work.pop() {
            loop {
                addr = past_sched(addr);
                if found.contains_key(&addr) {
                    break;
                }
                if found.len() >= MAX_INSTS {
                    return Err(format!("program at {entry:#x} is too long"));
                }
                let word = fetch(addr).ok_or_else(|| format!("no code at {addr:#x}"))?;
                let inst = decode(addr, word);
                found.insert(addr, (word, inst));
                if let Some(t) = target(&inst.op) {
                    work.push(t);
                }
                if ends(&inst) {
                    break;
                }
                addr = addr.wrapping_add(8);
            }
        }

        let addrs: Vec<u32> = found.keys().copied().collect();
        let index = |a: u32| addrs.binary_search(&a).map(|i| i as u32);
        let mut code = Vec::with_capacity(found.len());
        let mut words = Vec::with_capacity(found.len());
        let mut gprs = 0;
        for (&addr, &(word, mut inst)) in &found {
            if let Op::Bra { t, .. }
            | Op::Cal { t }
            | Op::Pret { t }
            | Op::Ssy { t }
            | Op::Pbk { t }
            | Op::Pcnt { t } = &mut inst.op
            {
                *t = index(*t).map_err(|_| format!("jump at {addr:#x} to {t:#x}"))?;
            }
            gprs = gprs.max(highest_reg(&inst.op));
            code.push(inst);
            words.push((addr, word));
        }
        let entry = index(entry).map_err(|_| "no entry".to_string())?;
        Ok(Program { code, words, high: Vec::new(), entry, gprs, jit: Default::default() })
    }

    /// Decodes everything reachable from code address `entry` of SM70+ code (128-bit
    /// instructions, no scheduling words). `fetch` returns the 64-bit word at a code address.
    /// An instruction may become two operations; jumps go to the first of them. A convergence
    /// barrier's meeting point (SSY) moves past the BSYNC (SYNC) at its label.
    pub fn decode_sm70_at(
        entry: u32,
        fetch: &mut dyn FnMut(u32) -> Option<u64>,
    ) -> Result<Program, String> {
        use crate::decode_sm70::{decode, SIZE};
        let mut found: BTreeMap<u32, (u128, Vec<Inst>)> = BTreeMap::new();
        let mut work = vec![entry];
        while let Some(mut addr) = work.pop() {
            loop {
                if found.contains_key(&addr) {
                    break;
                }
                if found.len() >= MAX_INSTS {
                    return Err(format!("program at {entry:#x} is too long"));
                }
                let lo = fetch(addr).ok_or_else(|| format!("no code at {addr:#x}"))?;
                let hi = fetch(addr + 8).ok_or_else(|| format!("no code at {:#x}", addr + 8))?;
                let raw = lo as u128 | (hi as u128) << 64;
                let mut insts = decode(addr, raw);
                if insts.is_empty() {
                    insts.push(Inst { pred: PT, op: Op::Nop });
                }
                let last = *insts.last().unwrap();
                for inst in &insts {
                    if let Some(t) = target(&inst.op) {
                        work.push(t);
                    }
                    // the code after a BSSY's BSYNC is reached only through the BSYNC
                    if let Op::Ssy { t } = inst.op {
                        work.push(t.wrapping_add(SIZE));
                    }
                }
                found.insert(addr, (raw, insts));
                if ends(&last) {
                    break;
                }
                addr = addr.wrapping_add(SIZE);
            }
        }

        // Uniform registers and the decoder's two scratch ones go above the program's vector
        // registers (by default R190 up): measure how far those go, and move them if needed,
        // packing the uniform registers the program names.
        {
            use crate::decode_sm70::{with_map, Map, SCRATCH};
            let measure = Map { uniform: [RZ; 64], scratch: 0, measure: true };
            let (mut vector, mut used) = (0, 0u64);
            for (&addr, (raw, _)) in &found {
                let (v, u) = with_map(measure, || decode(addr, *raw).iter().map(|i| highest_reg(&i.op)).max().unwrap_or(0));
                vector = vector.max(v);
                used |= u;
            }
            if vector > SCRATCH as u32 {
                let scratch = vector as Reg;
                let start = vector + 2;
                let Some(uniform) = Map::packed(start, used, 4).or_else(|| Map::packed(start, used, 2)) else {
                    let names: Vec<String> = (0..64).filter(|n| used >> n & 1 != 0).map(|n| format!("UR{n}")).collect();
                    return Err(format!("program at {entry:#x}: R0..R{} and {} do not all fit", vector - 1, names.join(" ")));
                };
                let map = Map { uniform, scratch, measure: false };
                let ((), again) = with_map(map, || {
                    for (&addr, (raw, insts)) in found.iter_mut() {
                        *insts = decode(addr, *raw);
                        if insts.is_empty() {
                            insts.push(Inst { pred: PT, op: Op::Nop });
                        }
                    }
                });
                debug_assert_eq!(again, used);
            }
        }

        let mut first = BTreeMap::new();
        let mut n = 0u32;
        for (&addr, (_, insts)) in &found {
            first.insert(addr, n);
            n += insts.len() as u32;
        }
        let index = |a: u32| first.get(&a).copied().ok_or(());
        let mut code = Vec::with_capacity(n as usize);
        let mut words = Vec::with_capacity(n as usize);
        let high = found.iter().map(|(&addr, (raw, _))| (addr + 8, (*raw >> 64) as u64)).collect();
        let mut gprs = 0;
        for (&addr, (raw, insts)) in &found {
            for &inst in insts {
                let mut inst = inst;
                if let Op::Bra { t, .. }
                | Op::Cal { t }
                | Op::Pret { t }
                | Op::Ssy { t }
                | Op::Pbk { t }
                | Op::Pcnt { t } = &mut inst.op
                {
                    *t = index(*t).map_err(|_| format!("jump at {addr:#x} to {t:#x}"))?;
                }
                gprs = gprs.max(highest_reg(&inst.op));
                code.push(inst);
                words.push((addr, *raw as u64));
            }
        }
        for k in 0..code.len() {
            if let Op::Ssy { t } = code[k].op {
                if matches!(code.get(t as usize).map(|i| i.op), Some(Op::Sync { .. })) {
                    code[k].op = Op::Ssy { t: t + 1 };
                }
            }
        }
        let entry = index(entry).map_err(|_| "no entry".to_string())?;
        Ok(Program { code, words, high, entry, gprs, jit: Default::default() })
    }

    /// Decodes a program at address 0 of `code`.
    pub fn decode(code: &[u8]) -> Result<Program, String> {
        Self::decode_at(0, &mut |a| {
            let a = a as usize;
            let b = code.get(a..a + 8)?;
            Some(u64::from_le_bytes(b.try_into().unwrap()))
        })
    }

    /// Whether the code in memory is still what was decoded.
    pub fn matches(&self, fetch: &mut dyn FnMut(u32) -> Option<u64>) -> bool {
        self.words.iter().chain(&self.high).all(|&(a, w)| fetch(a) == Some(w))
    }

    /// Code addresses (and words) this program was decoded from, in address order.
    pub fn words(&self) -> &[(u32, u64)] {
        &self.words
    }

    /// The instructions, in address order.
    pub fn insts(&self) -> &[Inst] {
        &self.code
    }

    /// Index of the entry point in `insts`.
    pub fn entry(&self) -> u32 {
        self.entry
    }

    /// Number of general registers the program writes (the highest, plus one).
    pub fn gprs(&self) -> u32 {
        self.gprs
    }

    /// The JIT's blocks of this program (compiled the first time), if the JIT is on.
    #[inline]
    pub fn jit(&self) -> Option<&crate::jit::Code> {
        if !crate::jit::enabled() {
            return None;
        }
        self.jit.get_or_init(|| crate::jit::code_for(self)).as_deref()
    }

    /// Whether the program has an instruction that satisfies `f` (for example: one that stores,
    /// so that invocations cannot be skipped).
    pub fn any(&self, f: impl Fn(&Op) -> bool) -> bool {
        self.code.iter().any(|i| f(&i.op))
    }

    /// Whether the program writes memory (stores, atomics, images).
    pub fn writes_memory(&self) -> bool {
        self.any(|op| {
            matches!(op, Op::St { .. } | Op::Atom { .. } | Op::Sust(_) | Op::Suatom { .. })
        })
    }

    /// Whether the program can discard fragments.
    pub fn kills(&self) -> bool {
        self.any(|op| matches!(op, Op::Kil { .. }))
    }

    pub(crate) fn index_of(&self, addr: u32) -> Option<u32> {
        self.words.binary_search_by_key(&past_sched(addr), |w| w.0).ok().map(|i| i as u32)
    }
}

fn target(op: &Op) -> Option<u32> {
    match *op {
        Op::Bra { t, .. }
        | Op::Cal { t }
        | Op::Pret { t }
        | Op::Ssy { t }
        | Op::Pbk { t }
        | Op::Pcnt { t } => Some(t),
        _ => None,
    }
}

/// Whether control never reaches the next instruction.
fn ends(i: &Inst) -> bool {
    let always = i.pred == PT;
    match i.op {
        Op::Exit { cond }
        | Op::Ret { cond }
        | Op::Sync { cond }
        | Op::Brk { cond }
        | Op::Cont { cond }
        | Op::Bra { cond, all: false, .. }
        | Op::Brx { cond, .. } => always && cond & 0x1f == 15,
        Op::Invalid(_) | Op::Bpt => true,
        _ => false,
    }
}

/// The highest register an instruction writes, plus one.
fn highest_reg(op: &Op) -> u32 {
    let (d, n) = match *op {
        Op::FAdd { d, .. }
        | Op::FMul { d, .. }
        | Op::Ffma { d, .. }
        | Op::FMnMx { d, .. }
        | Op::FCmp { d, .. }
        | Op::FSet { d, .. }
        | Op::FSwzAdd { d, .. }
        | Op::Mufu { d, .. }
        | Op::Half { d, .. }
        | Op::PackHalves { d, .. }
        | Op::Rro { d, .. }
        | Op::DSet { d, .. }
        | Op::IAdd { d, .. }
        | Op::IAdd3 { d, .. }
        | Op::IAdd3P { d, .. }
        | Op::IScAdd { d, .. }
        | Op::Lea { d, .. }
        | Op::Vadd { d, .. }
        | Op::P2r { d, .. }
        | Op::IMul { d, .. }
        | Op::IMad { d, .. }
        | Op::Xmad { d, .. }
        | Op::IMnMx { d, .. }
        | Op::ICmp { d, .. }
        | Op::ISet { d, .. }
        | Op::Lop { d, .. }
        | Op::Lop3 { d, .. }
        | Op::Shl { d, .. }
        | Op::Shr { d, .. }
        | Op::Shf { d, .. }
        | Op::Bfe { d, .. }
        | Op::Bfi { d, .. }
        | Op::Flo { d, .. }
        | Op::PopC { d, .. }
        | Op::Prmt { d, .. }
        | Op::Sel { d, .. }
        | Op::Mov { d, .. }
        | Op::PSet { d, .. }
        | Op::S2R { d, .. }
        | Op::Vote { d, .. }
        | Op::Shfl { d, .. }
        | Op::Isberd { d, .. }
        | Op::Ipa { d, .. }
        | Op::Pixld { d, .. }
        | Op::Out { d, .. } => (d, 1),
        Op::DAdd { d, .. } | Op::DMul { d, .. } | Op::Dfma { d, .. } | Op::DMnMx { d, .. } => {
            (d, 2)
        }
        Op::F2F { d, to, .. } | Op::I2F { d, to, .. } => (d, if to == 8 { 2 } else { 1 }),
        Op::F2I { d, to, .. } | Op::I2I { d, to, .. } => (d, if to.bytes == 8 { 2 } else { 1 }),
        Op::Ldc { d, ty, .. }
        | Op::Ld { d, ty, .. }
        | Op::Ldl { d, ty, .. }
        | Op::Lds { d, ty, .. } => (d, (ty.bytes() as u32).div_ceil(4)),
        Op::Atom { d, ty, .. } | Op::AtomS { d, ty, .. } | Op::Suatom { s: Surf { r: d, .. }, ty, .. } => {
            (d, ty.bytes() as u32 / 4)
        }
        Op::Ald { d, n, .. } | Op::Al2p { d, n, .. } => (d, n as u32),
        Op::Suld(s) => {
            (s.r, if s.raw { (s.ty.bytes() as u32).div_ceil(4) } else { s.mask.count_ones() })
        }
        Op::Tex(t) => {
            let n = t.mask.count_ones();
            if t.scalar {
                let hi = if t.d1 == RZ { 0 } else { t.d1 as u32 + n.saturating_sub(2) };
                let lo = if t.d0 == RZ { 0 } else { t.d0 as u32 + n.min(2) };
                return hi.max(lo);
            }
            (t.d0, n)
        }
        _ => return 0,
    };
    if d == RZ {
        0
    } else {
        d as u32 + n
    }
}
