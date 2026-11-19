//! The compute data master: a compute job (`struct rogue_fwif_cmd_compute`) runs the CDM
//! control stream at `cdm_ctrl_stream_base` (Mesa's `csbgen/rogue/cdm.xml`): compute kernel
//! blocks, links and a terminate. A kernel names a PDS program (data and code segments in
//! the PDS heap) that sets up and starts the USC tasks for its workgroups.
use crate::fw::Firmware;
use crate::vm::Vm;
use emu_core::FlatRam;

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
    let rd64 = |fw: &Firmware, off: u32| fw.fw_read(ram, cmd + off) as u64 | (fw.fw_read(ram, cmd + off + 4) as u64) << 32;
    let stream = rd64(fw, 8 + 32);
    let kernels = parse(ram, vm, stream);
    for k in &kernels {
        if fw.trace {
            eprintln!("[gpu cdm] {k:x?}");
            let dump = |vm: &mut Vm, base: u64, n: u64| -> String {
                (0..n).map(|i| vm.read32(ram, PDS_HEAP + base + 4 * i).map_or("--------".into(), |w| format!("{w:08x}"))).collect::<Vec<_>>().join(" ")
            };
            eprintln!("[gpu cdm]   pds data: {}", dump(vm, k.data_addr, 4 * k.pds_data_size as u64));
            eprintln!("[gpu cdm]   pds code: {}", dump(vm, k.code_addr, 32));
        }
    }
}
