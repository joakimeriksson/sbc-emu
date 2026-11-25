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
        Ok(Program { code, words, entry, gprs, jit: Default::default() })
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
        self.words.iter().all(|&(a, w)| fetch(a) == Some(w))
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
        | Op::Rro { d, .. }
        | Op::DSet { d, .. }
        | Op::IAdd { d, .. }
        | Op::IAdd3 { d, .. }
        | Op::IScAdd { d, .. }
        | Op::Lea { d, .. }
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
