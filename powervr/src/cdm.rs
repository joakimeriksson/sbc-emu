//! The compute data master: a compute job (`struct rogue_fwif_cmd_compute`) runs the CDM
//! control stream at `cdm_ctrl_stream_base` (Mesa's `csbgen/rogue/cdm.xml`): compute kernel
//! blocks, links and a terminate. A kernel names a PDS program (data and code segments in
//! the PDS heap) that sets up and starts the USC tasks for its workgroups.
//!
//! How a compute shader gets its inputs (Mesa's `pvr_pds_compute_shader`,
//! `pvr_compute_update_shared`, `pco_nir_compute.c`):
//! - push constants and descriptor addresses: a first kernel with `usc_common_shared` runs
//!   the pipeline's "descriptor" PDS program, which DMAs them (DOUTD) into the shared
//!   registers `sh*`;
//! - workgroup ID: the compute PDS program's coefficient-update branch (IF1) adds the base
//!   workgroup and DOUTWs the IDs the CDM put in its temps 0, 1, 3 into coefficients `cf*`;
//! - local invocation index (Mesa flattens the workgroup): the instance branch DOUTWs temp 0
//!   into the unified store, `vi*`, then DOUTU starts the shader with that many temps.
use crate::fw::Firmware;
use crate::pds::{self, Pds};
use crate::usc;
use crate::vm::Vm;
use emu_core::FlatRam;
use std::collections::HashMap;

/// Where the PDS and USC code heaps sit in every GPU virtual address space
/// (`sysinit.pds_exec_base`, `usc_exec_base`).
pub const PDS_HEAP: u64 = 0xda_0000_0000;
pub const USC_HEAP: u64 = 0xe0_0000_0000;

/// One `COMPUTE_KERNEL` block.
#[derive(Clone, Debug, Default)]
pub struct Kernel {
    pub usc_common_size: u32,
    pub usc_unified_size: u32,
    pub pds_temp_size: u32,
    pub pds_data_size: u32,
    pub usc_target_any: bool,
    pub fence: bool,
    /// PDS data and code segments (offsets in the PDS heap)
    pub data_addr: u64,
    pub code_addr: u64,
    /// shared-data type (0 none, 1 PDS, 2 USC) and whether it fills the common store
    pub sd_type: u32,
    pub usc_common_shared: bool,
    pub one_wg_per_task: bool,
    /// workgroup counts (the stream holds count - 1)
    pub groups: [u32; 3],
    pub indirect: Option<u64>,
    pub max_instances: u32,
    /// workgroup size (the stream holds size - 1)
    pub size: [u32; 3],
    pub offsets: [u32; 3],
}

/// Decode the control stream at `va` into its kernels.
pub fn parse(ram: &FlatRam, vm: &mut Vm, mut va: u64) -> Vec<Kernel> {
    let mut out = Vec::new();
    let next = |vm: &mut Vm, va: &mut u64| {
        let w = vm.read32(ram, *va).unwrap_or(2 << 30);
        *va += 4;
        w
    };
    for _ in 0..4096 {
        let w0 = next(vm, &mut va);
        match w0 >> 30 {
            0 => {
                let mut k = Kernel {
                    usc_common_size: (w0 >> 18) & 0x1ff,
                    usc_unified_size: (w0 >> 12) & 0x3f,
                    pds_temp_size: (w0 >> 8) & 0xf,
                    pds_data_size: (w0 >> 2) & 0x3f,
                    usc_target_any: w0 & 2 != 0,
                    fence: w0 & 1 != 0,
                    ..Default::default()
                };
                let (indirect, offsets) = (w0 & 1 << 29 != 0, w0 & 1 << 28 != 0);
                let w1 = next(vm, &mut va);
                k.data_addr = (w1 & !0xf) as u64;
                k.sd_type = (w1 >> 2) & 3;
                k.usc_common_shared = w1 & 2 != 0;
                let w2 = next(vm, &mut va);
                k.code_addr = (w2 & !0xf) as u64;
                k.one_wg_per_task = w2 & 1 != 0;
                for g in 0..3 {
                    k.groups[g] = next(vm, &mut va).wrapping_add(1);
                }
                if indirect {
                    let msb = (next(vm, &mut va) & 0xff) as u64;
                    let lsb = (next(vm, &mut va) & !3) as u64;
                    k.indirect = Some(msb << 32 | lsb);
                }
                let w8 = next(vm, &mut va);
                k.max_instances = w8 >> 27;
                k.size = [((w8 >> 18) & 0x1ff) + 1, ((w8 >> 9) & 0x1ff) + 1, (w8 & 0x1ff) + 1];
                if offsets {
                    for g in 0..3 {
                        k.offsets[g] = next(vm, &mut va);
                    }
                }
                out.push(k);
            }
            1 => {
                let msb = (w0 & 0xff) as u64;
                let lsb = (next(vm, &mut va) & !3) as u64;
                va = msb << 32 | lsb;
            }
            _ => break,
        }
    }
    out
}

/// `struct rogue_fwif_cmd_compute` at `cmd`: regs at +8.
pub fn run(fw: &mut Firmware, ram: &mut FlatRam, vm: &mut Vm, cmd: u32) {
    let rd64 = |fw: &Firmware, off: u32| fw.fw_read(ram, cmd.wrapping_add(off)) as u64 | (fw.fw_read(ram, cmd.wrapping_add(off + 4)) as u64) << 32;
    let stream = rd64(fw, 8 + 32);
    let kernels = parse(ram, vm, stream);
    let trace = fw.trace;
    let mut mem = GpuMem { ram, vm };
    let mut cdm = Cdm {
        shareds: vec![0; SHARED_WORDS],
        pds: Pds { trace, ..Default::default() },
        cache: HashMap::new(),
        trace,
        faults: 0,
        invocations: 0,
    };
    for k in &kernels {
        if trace {
            eprintln!("[gpu cdm] {k:x?}");
            let data: Vec<String> = (0..4 * k.pds_data_size as u64)
                .map(|i| mem.read32(PDS_HEAP + k.data_addr + 4 * i).map_or("--------".into(), |w| format!("{w:08x}")))
                .collect();
            eprintln!("[gpu cdm]   pds data: {}", data.join(" "));
        }
        cdm.kernel(&mut mem, k);
    }
    fw.invocations += cdm.invocations;
    fw.gpu_faults += cdm.faults;
    if trace {
        eprintln!("[gpu cdm] job done: {} kernels, {} invocations, {} faults", kernels.len(), cdm.invocations, cdm.faults);
    }
}

/// The largest workgroup (instances) and dispatch (workgroups) a kernel may ask for.
const MAX_WORKGROUP_INSTANCES: usize = 4096;
const MAX_WORKGROUPS: u64 = 1 << 24;

/// Words of the shared part of the common store (shared registers come first in the
/// cluster's common store; Mesa allocates at most 1024 dwords of them).
const SHARED_WORDS: usize = 4096;

/// GPU memory through the job's address space.
pub(crate) struct GpuMem<'a> {
    pub(crate) ram: &'a mut FlatRam,
    pub(crate) vm: &'a mut Vm,
}

impl usc::Memory for GpuMem<'_> {
    fn read32(&mut self, va: u64) -> Option<u32> {
        self.vm.read32(self.ram, va)
    }

    fn write32(&mut self, va: u64, v: u32) -> bool {
        self.vm.write32(self.ram, va, v)
    }
}

impl GpuMem<'_> {
    pub(crate) fn read32(&mut self, va: u64) -> Option<u32> {
        self.vm.read32(self.ram, va)
    }
}

/// The CDM's state across the kernels of one job.
struct Cdm {
    /// shared registers (`sh*` in the USC): loaded by a kernel with `usc_common_shared`,
    /// read by the kernels after it
    shareds: Vec<u32>,
    pds: Pds,
    /// decoded USC groups
    cache: HashMap<u64, Result<usc::Group, usc::DecodeError>>,
    trace: bool,
    faults: u64,
    invocations: u64,
}

/// Where a PDS task's output goes: the common store (shared registers or a workgroup's
/// coefficients) and the unified store of the instance it runs for.
pub(crate) struct TaskHost<'a, 'b> {
    pub(crate) mem: &'a mut GpuMem<'b>,
    pub(crate) common: &'a mut [u32],
    pub(crate) unified: &'a mut [u32],
    pub(crate) doutu: Option<pds::DoutU>,
    /// DOUTI issues (iterator / coefficient set-ups) in program order
    pub(crate) douti: Vec<u64>,
    pub(crate) trace: bool,
}

impl TaskHost<'_, '_> {
    fn store(&mut self, common: bool, at: u32, v: u32) {
        let (store, name) = if common { (&mut *self.common, "common") } else { (&mut *self.unified, "unified") };
        match store.get_mut(at as usize) {
            Some(s) => *s = v,
            None if self.trace => eprintln!("[gpu pds] write beyond the {name} store: {at}"),
            None => {}
        }
    }
}

impl pds::Host for TaskHost<'_, '_> {
    fn read32(&mut self, va: u64) -> Option<u32> {
        self.mem.read32(va)
    }

    fn write32(&mut self, va: u64, v: u32) -> bool {
        usc::Memory::write32(self.mem, va, v)
    }

    fn doutd(&mut self, d: pds::DoutD) {
        for i in 0..d.size {
            let v = self.mem.read32(d.addr + 4 * i as u64);
            if v.is_none() && self.trace {
                eprintln!("[gpu pds] doutd from unmapped {:#x}", d.addr + 4 * i as u64);
            }
            self.store(d.common, d.ao + i, v.unwrap_or(0));
        }
    }

    fn doutw(&mut self, w: pds::DoutW) {
        for (i, v) in w.words.iter().enumerate() {
            if let Some(v) = v {
                self.store(w.common, w.ao + i as u32, *v);
            }
        }
    }

    fn doutu(&mut self, u: pds::DoutU) {
        self.doutu = Some(u);
    }

    fn dout_other(&mut self, kind: pds::DoutKind, src0: u64, src1: u32) {
        if kind == pds::DoutKind::I {
            self.douti.push(src0);
        } else if self.trace {
            eprintln!("[gpu pds] dout {kind:?} {src0:#x} {src1:#x} not modelled");
        }
    }
}

impl Cdm {
    /// Run one kernel the way the CDM does:
    ///
    /// - A kernel with `usc_common_shared` set fills (or frees) the shared registers: its PDS
    ///   program runs once per task, and its writes to the common store land in the shared
    ///   area that the following kernels read as `sh*`.
    /// - Otherwise each workgroup first gets a *coefficient update task* (the PDS program
    ///   with IF1 set and the workgroup ID in temps 0, 1 and 3), whose common-store writes
    ///   fill that workgroup's coefficient registers (`cf*`); then its *instance tasks* (IF1
    ///   clear, the flat local invocation index in temp 0), whose unified-store writes land
    ///   in each instance's `vi*` registers and whose DOUTU starts the USC program.
    ///
    /// IF0 is set for every task (cc-gated instructions such as the DOUTU run). The
    /// hardware runs the instance program once per task of up to 32 instances; running it
    /// per instance gives each its own unified store, which is what the DOUTW of the local
    /// index means. The USC then runs a whole workgroup in lockstep, so barriers need no
    /// extra care.
    fn kernel(&mut self, mem: &mut GpuMem, k: &Kernel) {
        let data: Vec<u32> = (0..4 * k.pds_data_size as u64).map(|i| mem.read32(PDS_HEAP + k.data_addr + 4 * i).unwrap_or(0)).collect();
        let code = PDS_HEAP + k.code_addr;
        if self.trace {
            let words: Vec<u32> = (0..256).map_while(|i| mem.read32(code + 4 * i)).collect();
            for l in pds::disassemble(&words[..pds::program_len(&words)]).lines() {
                eprintln!("[gpu pds]   {l}");
            }
        }
        let groups = match k.indirect {
            Some(addr) => {
                let g: Vec<u32> = (0..3).map(|i| mem.read32(addr.wrapping_add(4 * i)).unwrap_or(0)).collect();
                [g[0], g[1], g[2]]
            }
            None => k.groups,
        };
        let local = (k.size[0] * k.size[1] * k.size[2]) as usize;
        // guest-controlled sizes: refuse what no Vulkan dispatch produces rather than
        // exhausting host memory or time
        let total = groups.iter().map(|&g| g as u64).product::<u64>();
        if local > MAX_WORKGROUP_INSTANCES || total > MAX_WORKGROUPS {
            self.fault(format!("dispatch of {groups:?} workgroups of {local} instances is beyond the emulator's limits"));
            return;
        }
        // KERNEL0 sizes: common store in 64-byte units, unified store in 16-byte units
        let coeff_words = (16 * k.usc_common_size as usize).max(16);
        let unified_words = (4 * k.usc_unified_size as usize).max(4);
        let coeff_task = !k.usc_common_shared && k.usc_common_size > 0;
        if k.offsets != [0; 3] && self.trace {
            eprintln!("[gpu cdm] global offsets {:?} ignored", k.offsets);
        }
        for z in 0..groups[2] {
            for y in 0..groups[1] {
                for x in 0..groups[0] {
                    // the PDS runs of the first workgroup are traced
                    self.pds.trace = self.trace && x == 0 && y == 0 && z == 0;
                    let mut coeffs = vec![0u32; coeff_words];
                    if coeff_task {
                        let task = pds::Task { if0: true, if1: true, temps: vec![x, y, 0, z] };
                        let mut none = [];
                        let mut host = TaskHost { mem, common: &mut coeffs, unified: &mut none, doutu: None, douti: Vec::new(), trace: false };
                        let r = self.pds.run(&mut host, code, &data, &task);
                        let kick = host.doutu;
                        if let Err(e) = r {
                            self.fault(format!("PDS coefficient task: {e:?}"));
                            return;
                        }
                        if let Some(u) = kick {
                            // the coefficient update program runs once for the workgroup
                            let mut insts = vec![usc::Instance::new((u.temps as usize).max(8), unified_words)];
                            if !self.usc(mem, u, &mut coeffs, &mut insts) {
                                return;
                            }
                        }
                    }
                    let mut insts = Vec::with_capacity(local);
                    let mut kick = None;
                    for i in 0..local {
                        let task = pds::Task { if0: true, if1: false, temps: vec![i as u32, 0] };
                        let mut unified = vec![0u32; unified_words];
                        let common = if k.usc_common_shared { &mut self.shareds[..] } else { &mut coeffs[..] };
                        let mut host = TaskHost { mem, common, unified: &mut unified, doutu: None, douti: Vec::new(), trace: self.trace };
                        let r = self.pds.run(&mut host, code, &data, &task);
                        kick = kick.or(host.doutu);
                        // ... and of those, the first instance task
                        self.pds.trace = false;
                        if let Err(e) = r {
                            self.fault(format!("PDS instance task: {e:?}"));
                            return;
                        }
                        let mut inst = usc::Instance::new(8, 0);
                        inst.vtxin = unified;
                        insts.push(inst);
                    }
                    if let Some(u) = kick {
                        for inst in &mut insts {
                            inst.temps = vec![0; (u.temps as usize).max(8)];
                        }
                        if !self.usc(mem, u, &mut coeffs, &mut insts) {
                            return;
                        }
                        self.invocations += insts.len() as u64;
                    }
                }
            }
        }
    }

    /// Run a USC program for `insts`; false if it faulted.
    fn usc(&mut self, mem: &mut GpuMem, u: pds::DoutU, coeffs: &mut [u32], insts: &mut [usc::Instance]) -> bool {
        let entry = USC_HEAP + u.exe_offset as u64;
        let mut m = usc::Machine { mem, coeffs, shareds: &mut self.shareds, cache: &mut self.cache, trace: self.trace, emits: Vec::new(), quad: Vec::new() };
        match m.run(entry, insts) {
            Ok(_) => true,
            Err(f) => {
                self.fault(format!("USC program {entry:#x}: fault at {:#x}: {}", f.pc, f.msg));
                false
            }
        }
    }

    /// An unimplemented instruction or a bad access: the kernel is abandoned. Reported
    /// under trace, and once per job otherwise.
    fn fault(&mut self, msg: String) {
        if self.trace || self.faults == 0 {
            eprintln!("[gpu cdm] {msg}");
        }
        self.faults += 1;
    }
}
