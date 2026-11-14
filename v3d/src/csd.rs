//! The compute shader dispatcher: workgroups packed into supergroups, supergroups into
//! batches of 16 invocations (one QPU thread each). Threads of a supergroup run together so
//! that barriers (SYNCB writes) can be honoured.
//!
//! Configuration (CSD_QUEUED_CFG0-6): workgroup counts and offsets (CFG0-2), workgroup size /
//! workgroups per supergroup / batches per supergroup (CFG3), batch count - 1 (CFG4), shader
//! address with threading/single-segment flags (CFG5) and the uniforms address (CFG6).
//!
//! Payload (Mesa `nir_to_vir`): rf0 = workgroup x | y << 16; rf2 = workgroup z | index of the
//! workgroup within its supergroup << 16 | local invocation index in the top bits.

use crate::qpu::{Env, Stop, Thread, TmuKind, TmuReq, LANES, V};
use crate::{Code, Gmem};

pub struct CsEnv<'a, 'm> { pub m: &'a mut Gmem<'m> }

impl Env for CsEnv<'_, '_> {
    fn read32(&mut self, va: u32) -> u32 { self.m.r32(va) }
    fn write32(&mut self, va: u32, v: u32) { self.m.w32(va, v) }
    fn tmu(&mut self, t: &Thread, kind: TmuKind, cfg: &[u32], req: &TmuReq, lanes: u16) -> Vec<V> {
        crate::tmu::lookup(self.m, t, kind, cfg, req, lanes, false)
    }
}

pub fn run(cfg: &[u32], m: &mut Gmem, code: &mut Code, insns: &mut u64) {
    let n = [cfg[0] >> 16, cfg[1] >> 16, cfg[2] >> 16];
    let o = [cfg[0] & 0xffff, cfg[1] & 0xffff, cfg[2] & 0xffff];
    let wg_size = match cfg[3] & 0xff { 0 => 256, s => s };
    let wgs_per_sg = match (cfg[3] >> 8) & 0xf { 0 => 16, s => s };
    let shader = cfg[5] & !7;
    let single_seg = cfg[5] & 2 != 0;
    let unif = cfg[6];
    let total_wgs = n[0] as u64 * n[1] as u64 * n[2] as u64;
    if total_wgs == 0 { return; }
    let local_bits = (wg_size.max(64)).next_power_of_two().trailing_zeros();
    crate::log(&format!("v3d: CSD {}x{}x{} groups of {wg_size}, {wgs_per_sg} per supergroup, shader {shader:#x}", n[0], n[1], n[2]));
    let mut env = CsEnv { m };
    let mut wg = 0u64;
    while wg < total_wgs {
        let in_sg = (wgs_per_sg as u64).min(total_wgs - wg) as u32;
        let invocations = in_sg * wg_size;
        let batches = (invocations + 15) / 16;
        let mut threads: Vec<Thread> = (0..batches).map(|b| {
            let mut t = Thread::new(shader, unif, single_seg);
            t.valid = 0;
            t.tidx = b;
            for l in 0..LANES as u32 {
                let i = b * 16 + l;
                if i >= invocations { t.msf[l as usize] = 0; continue; }
                t.valid |= 1 << l;
                let (g, local) = (wg + (i / wg_size) as u64, i % wg_size);
                let x = (g % n[0] as u64) as u32 + o[0];
                let y = ((g / n[0] as u64) % n[1] as u64) as u32 + o[1];
                let z = (g / (n[0] as u64 * n[1] as u64)) as u32 + o[2];
                t.rf[0][l as usize] = (x & 0xffff) | (y << 16);
                t.rf[2][l as usize] = (z & 0xffff) | ((i / wg_size) << 16) | (local << (32 - local_bits));
            }
            t
        }).collect();
        loop {
            let mut live = false;
            for t in threads.iter_mut().filter(|t| !t.done) {
                let before = t.insns;
                match t.run(code, &mut env, 50_000_000) {
                    Stop::Barrier => live = true,
                    Stop::Done => {}
                    Stop::Budget | Stop::Fault => { crate::log_once("v3d: compute shader did not finish"); t.done = true; }
                }
                *insns += t.insns - before;
            }
            if !live { break; }
        }
        wg += in_sg as u64;
    }
}
