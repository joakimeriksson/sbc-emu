//! The GPU firmware's side of the interface Linux's `powervr` driver talks to
//! (`pvr_rogue_fwif.h`), played by the emulator instead of running firmware on the GPU's MIPS
//! core.
//!
//! Firmware addresses (0xC000_0000 + offset into the firmware heap) go through the driver's
//! MIPS page table, found through the boot data the driver points MIPS_ADDR_REMAP2 at. The
//! init structures sit at fixed places at the top of the 16 MiB heap.
use emu_core::FlatRam;

const FW_BASE: u32 = 0xc000_0000;
const CONNECTION_CTL: u32 = 0xc0fd_0000;
const OSINIT: u32 = 0xc0fe_0000;
const SYSINIT: u32 = 0xc0ff_0000;

// struct rogue_fwif_sysinit
const SYSINIT_FW_SYS_DATA: u32 = 324;
const SYSINIT_FIRMWARE_STARTED: u32 = 352;
const SYSINIT_MARKER: u32 = 356;
// struct rogue_fwif_osinit
const OSINIT_KCCB_CTL: u32 = 0;
const OSINIT_KCCB: u32 = 4;
const OSINIT_KCCB_RTN: u32 = 8;
const OSINIT_FW_OS_DATA: u32 = 36;
// struct rogue_fwif_osdata
const OSDATA_INTERRUPT_COUNT: u32 = 560;
const OSDATA_KCCB_CMDS_EXECUTED: u32 = 568;
const OSDATA_POWER_SYNC: u32 = 572;
// struct rogue_fwif_sysdata
const SYSDATA_POW_STATE: u32 = 8;
// struct rogue_fwif_ccb_ctl
const CCB_WRITE_OFFSET: u32 = 0;
const CCB_READ_OFFSET: u32 = 128;
const CCB_WRAP_MASK: u32 = 132;
const KCCB_CMD_SIZE: u32 = 88;
/// At most this many commands are taken from a ring at a time (guest-controlled offsets
/// must not hang the emulator).
const MAX_RING_STEPS: u32 = 4096;
/// The largest client command accepted (the biggest, a FRAG, is 480 bytes).
const MAX_CCB_CMD_SIZE: u32 = 64 * 1024;

const POW_OFF: u32 = 0;
const POW_FORCED_IDLE: u32 = 2;
const POW_IDLE: u32 = 3;

pub const KCCB_CMD_KICK: u32 = 0x2abc_0065;
pub const KCCB_CMD_MMUCACHE: u32 = 0x2abc_0066;
pub const KCCB_CMD_CLEANUP: u32 = 0x2abc_006a;
pub const KCCB_CMD_POW: u32 = 0x2abc_006b;
pub const KCCB_CMD_HEALTH_CHECK: u32 = 0x2abc_0073;
pub const KCCB_CMD_COMBINED_GEOM_FRAG_KICK: u32 = 0x2abc_0075;
const RTN_EXECUTED: u32 = 1;

// struct rogue_fwif_fwcommoncontext
const CTX_CCBCTL: u32 = 0;
const CTX_CCB: u32 = 4;
const CTX_MEM_CONTEXT: u32 = 136;
// struct rogue_fwif_cccb_ctl
const CCCB_WRITE_OFFSET: u32 = 0;
const CCCB_READ_OFFSET: u32 = 4;
const CCCB_WRAP_MASK: u32 = 12;
/// struct rogue_fwif_ccb_cmd_header
const CMD_HEADER_SIZE: u32 = 40;

// job commands: 201.. with the magic and ROGUE_CCB_TYPE_TASK (bit 15)
pub const CCB_CMD_GEOM: u32 = 0x2abc_80c9;
pub const CCB_CMD_FRAG: u32 = 0x2abc_80cb;
pub const CCB_CMD_FRAG_PR: u32 = 0x2abc_80cc;
pub const CCB_CMD_TQ_3D: u32 = 0x2abc_80ca;
pub const CCB_CMD_CDM: u32 = 0x2abc_80cd;
pub const CCB_CMD_FENCE: u32 = 0x2abc_00d4;
pub const CCB_CMD_UPDATE: u32 = 0x2abc_00d5;
pub const CCB_CMD_FENCE_PR: u32 = 0x2abc_00d7;
pub const CCB_CMD_UNFENCED_UPDATE: u32 = 0x2abc_00dc;

#[derive(Default)]
pub struct Firmware {
    /// running (between the driver releasing the MIPS core and a power-off or reset)
    pub running: bool,
    /// physical address of the boot data (`struct rogue_mipsfw_boot_data`)
    boot_data: u64,
    kccb_ctl: u32,
    kccb: u32,
    kccb_rtn: u32,
    osdata: u32,
    sysdata: u32,
    /// an interrupt to the host is due
    irq: bool,
    /// statistics
    pub boots: u64,
    pub commands: u64,
    pub jobs: u64,
    /// compute invocations run, and GPU programs abandoned on an unimplemented instruction
    pub invocations: u64,
    pub gpu_faults: u64,
    /// the 3D pipeline: scenes between their geometry and fragment jobs
    pub gfx: crate::gfx::Gfx,
    pub trace: bool,
    /// trace only the first this many GPU jobs (SBC_GPU_TRACE_JOBS; 0 = all)
    pub trace_jobs: u64,
}

impl Firmware {
    /// A firmware address to a physical one, through the driver's MIPS page table:
    /// 4096 32-bit entries over the four pages named in the boot data; an entry is valid with
    /// bit 1 set and holds the page frame in bits 29:6.
    fn translate(&self, ram: &FlatRam, fw_addr: u32) -> Option<u64> {
        let idx = fw_addr.checked_sub(FW_BASE)? as u64 >> 12;
        if idx >= 4096 {
            return None;
        }
        let pt_page = read64(ram, self.boot_data.wrapping_add(16 + 8 * (idx / 1024)))?;
        let pte = read32(ram, (pt_page & 0xff_ffff_f000) + 4 * (idx % 1024))?;
        if pte & 2 == 0 {
            return None;
        }
        Some((((pte as u64) & 0x3fff_ffc0) >> 6) << 12 | (fw_addr & 0xfff) as u64)
    }

    fn rd(&self, ram: &FlatRam, fw_addr: u32) -> u32 {
        self.translate(ram, fw_addr).and_then(|pa| read32(ram, pa)).unwrap_or(0)
    }

    fn wr(&self, ram: &mut FlatRam, fw_addr: u32, v: u32) {
        if let Some(pa) = self.translate(ram, fw_addr) {
            let _ = ram.load(pa, &v.to_le_bytes());
        } else if self.trace {
            eprintln!("[gpu fw] write to unmapped firmware address {fw_addr:#x}");
        }
    }

    /// The driver released the MIPS core: find the init structures, report the firmware
    /// started and idle, and serve anything already queued.
    pub fn boot(&mut self, ram: &mut FlatRam, boot_data: u64) {
        self.boot_data = boot_data;
        if self.rd(ram, SYSINIT + SYSINIT_MARKER) != 1 {
            if self.trace {
                eprintln!("[gpu fw] boot: no sysinit at {SYSINIT:#x} (boot data {boot_data:#x})");
            }
            return;
        }
        self.kccb_ctl = self.rd(ram, OSINIT + OSINIT_KCCB_CTL);
        self.kccb = self.rd(ram, OSINIT + OSINIT_KCCB);
        self.kccb_rtn = self.rd(ram, OSINIT + OSINIT_KCCB_RTN);
        self.osdata = self.rd(ram, OSINIT + OSINIT_FW_OS_DATA);
        self.sysdata = self.rd(ram, SYSINIT + SYSINIT_FW_SYS_DATA);
        // the connection is active (ROGUE_FWIF_CONNECTION_FW_ACTIVE)
        self.wr(ram, CONNECTION_CTL, 2);
        self.wr(ram, self.sysdata.wrapping_add(SYSDATA_POW_STATE), POW_IDLE);
        self.running = true;
        self.boots = self.boots.wrapping_add(1);
        // firmware_started last: it is what the driver waits for
        self.wr(ram, SYSINIT + SYSINIT_FIRMWARE_STARTED, 1);
        if self.trace {
            eprintln!("[gpu fw] booted: kccb ctl {:#x} cmds {:#x} rtn {:#x}", self.kccb_ctl, self.kccb, self.kccb_rtn);
        }
        self.doorbell(ram);
    }

    pub fn halt(&mut self) {
        self.running = false;
    }

    /// The host rang the kernel command ring: execute everything queued.
    ///
    /// Everything here comes from guest memory, so the walk is bounded: at most one ring's
    /// worth of commands (`MAX_RING_STEPS`), offsets masked to the ring, and it stops if
    /// the read offset would not move (a zero wrap mask).
    pub fn doorbell(&mut self, ram: &mut FlatRam) {
        if !self.running {
            return;
        }
        let mut any = false;
        for _ in 0..MAX_RING_STEPS {
            let (wr, rd) = (self.rd(ram, self.kccb_ctl.wrapping_add(CCB_WRITE_OFFSET)), self.rd(ram, self.kccb_ctl.wrapping_add(CCB_READ_OFFSET)));
            if wr == rd || !self.running {
                break;
            }
            let wrap = self.rd(ram, self.kccb_ctl.wrapping_add(CCB_WRAP_MASK));
            let rd = rd & wrap;
            let next = rd.wrapping_add(1) & wrap;
            let cmd = self.kccb.wrapping_add(rd.wrapping_mul(KCCB_CMD_SIZE));
            let rtn = self.command(ram, cmd);
            self.wr(ram, self.kccb_rtn.wrapping_add(rd.wrapping_mul(4)), rtn);
            self.wr(ram, self.kccb_ctl.wrapping_add(CCB_READ_OFFSET), next);
            let n = self.rd(ram, self.osdata.wrapping_add(OSDATA_KCCB_CMDS_EXECUTED));
            self.wr(ram, self.osdata.wrapping_add(OSDATA_KCCB_CMDS_EXECUTED), n.wrapping_add(1));
            self.commands = self.commands.wrapping_add(1);
            any = true;
            if next == rd {
                self.ring_fault("kernel command ring with a zero wrap mask");
                break;
            }
        }
        if any {
            let n = self.rd(ram, self.osdata.wrapping_add(OSDATA_INTERRUPT_COUNT));
            self.wr(ram, self.osdata.wrapping_add(OSDATA_INTERRUPT_COUNT), n.wrapping_add(1));
            self.irq = true;
        }
    }

    /// A malformed command ring: counted as a GPU fault, reported once.
    fn ring_fault(&mut self, msg: &str) {
        if self.trace || self.gpu_faults == 0 {
            eprintln!("[gpu fw] {msg}");
        }
        self.gpu_faults = self.gpu_faults.wrapping_add(1);
    }

    /// One kernel command; returns its return-slot flags.
    fn command(&mut self, ram: &mut FlatRam, cmd: u32) -> u32 {
        let at = |off: u32| cmd.wrapping_add(off);
        let ty = self.rd(ram, cmd);
        if self.trace {
            eprintln!("[gpu fw] kccb command {ty:#x}");
        }
        match ty {
            KCCB_CMD_POW => {
                let state = match self.rd(ram, at(8)) {
                    1 => POW_OFF,
                    2 => POW_FORCED_IDLE,
                    _ => POW_IDLE,
                };
                self.wr(ram, self.sysdata.wrapping_add(SYSDATA_POW_STATE), state);
                let sync = self.rd(ram, self.osdata.wrapping_add(OSDATA_POWER_SYNC));
                self.wr(ram, sync, 1);
                if state == POW_OFF {
                    self.running = false;
                }
            }
            KCCB_CMD_KICK => self.kick(ram, at(8)),
            KCCB_CMD_COMBINED_GEOM_FRAG_KICK => {
                self.kick(ram, at(8));
                self.kick(ram, at(40));
            }
            KCCB_CMD_MMUCACHE => {
                // nothing is cached: translations are read from memory each time
                let (sync, value) = (self.rd(ram, at(12)), self.rd(ram, at(16)));
                if sync != 0 {
                    self.wr(ram, sync, value);
                }
            }
            // health check, cleanup (never busy), log type, free-list replies: acknowledged
            _ => {}
        }
        RTN_EXECUTED
    }

    /// A context's client command ring has new commands (`struct rogue_fwif_kccb_cmd_kick_data`
    /// at `kick`): run them in order.
    ///
    /// The walk is bounded (`MAX_RING_STEPS`); a command whose size is not a multiple of 4,
    /// larger than `MAX_CCB_CMD_SIZE` or larger than the ring, or one that would not move
    /// the read offset, ends it (the ring is consumed up to the write offset) with a GPU
    /// fault.
    fn kick(&mut self, ram: &mut FlatRam, kick: u32) {
        let ctx = self.rd(ram, kick);
        let (ctl, ccb, mem_ctx) = (
            self.rd(ram, ctx.wrapping_add(CTX_CCBCTL)),
            self.rd(ram, ctx.wrapping_add(CTX_CCB)),
            self.rd(ram, ctx.wrapping_add(CTX_MEM_CONTEXT)),
        );
        // the page catalogue of the context's GPU virtual address space
        let pc = self.translate(ram, mem_ctx).and_then(|pa| read64(ram, pa)).unwrap_or(0);
        // the kick carries the new write offset and wrap mask; the firmware keeps them in the
        // control block
        let (woff, wrap) = (self.rd(ram, kick.wrapping_add(4)), self.rd(ram, kick.wrapping_add(8)));
        self.wr(ram, ctl.wrapping_add(CCCB_WRITE_OFFSET), woff);
        self.wr(ram, ctl.wrapping_add(CCCB_WRAP_MASK), wrap);
        let mut roff = self.rd(ram, ctl.wrapping_add(CCCB_READ_OFFSET)) & wrap;
        let woff = woff & wrap;
        let mut steps = 0;
        while roff != woff {
            steps += 1;
            if steps > MAX_RING_STEPS {
                self.ring_fault("client command ring: too many commands in one kick");
                roff = woff;
                break;
            }
            let hdr = ccb.wrapping_add(roff);
            let (ty, size) = (self.rd(ram, hdr), self.rd(ram, hdr.wrapping_add(4)));
            let body = hdr.wrapping_add(CMD_HEADER_SIZE);
            if self.trace {
                eprintln!("[gpu fw] ctx {ctx:#x} ccb cmd {ty:#x} size {size}");
            }
            if size % 4 != 0 || size > MAX_CCB_CMD_SIZE || size as u64 + CMD_HEADER_SIZE as u64 > wrap as u64 + 1 {
                self.ring_fault(&format!("client command {ty:#x} with a bad size {size}"));
                roff = woff;
                break;
            }
            match ty {
                // all earlier work is complete when a command is reached: fences are met
                CCB_CMD_FENCE | CCB_CMD_FENCE_PR => {}
                CCB_CMD_UPDATE | CCB_CMD_UNFENCED_UPDATE => {
                    for i in 0..size / 8 {
                        let (addr, value) = (self.rd(ram, body.wrapping_add(8 * i)), self.rd(ram, body.wrapping_add(8 * i + 4)));
                        self.wr(ram, addr, value);
                    }
                }
                CCB_CMD_CDM => {
                    let mut vm = crate::vm::Vm::new(pc);
                    let trace = self.trace;
                    self.trace = self.job_trace();
                    crate::cdm::run(self, ram, &mut vm, body);
                    self.trace = trace;
                    self.jobs = self.jobs.wrapping_add(1);
                }
                // a partial render: only needed when the parameter buffer runs out, which it
                // never does here; its fences and updates are the commands around it
                CCB_CMD_FRAG_PR => {
                    self.jobs = self.jobs.wrapping_add(1);
                }
                CCB_CMD_GEOM | CCB_CMD_FRAG | CCB_CMD_TQ_3D => {
                    let words: Vec<u32> = (0..size / 4).map(|i| self.rd(ram, body.wrapping_add(4 * i))).collect();
                    // the render size: the HWRT data's common part, screen_pixel_max
                    let screen = match (ty, words.get(1)) {
                        (CCB_CMD_FRAG, Some(&hwrt)) => {
                            let common = self.rd(ram, hwrt.wrapping_add(164));
                            self.rd(ram, common.wrapping_add(4))
                        }
                        _ => 0,
                    };
                    let mut vm = crate::vm::Vm::new(pc);
                    let mut mem = crate::cdm::GpuMem { ram, vm: &mut vm };
                    let trace = self.job_trace();
                    let r = match ty {
                        CCB_CMD_GEOM => self.gfx.geom(&mut mem, &words, trace),
                        CCB_CMD_FRAG => self.gfx.frag(&mut mem, &words, screen, trace),
                        _ => self.gfx.transfer(&mut mem, &words, trace),
                    };
                    if let Err(e) = r {
                        if trace || self.gpu_faults == 0 {
                            eprintln!("[gpu gfx] job {ty:#x}: {e}");
                        }
                        self.gpu_faults = self.gpu_faults.wrapping_add(1);
                    }
                    self.jobs = self.jobs.wrapping_add(1);
                }
                _ => {}
            }
            let next = roff.wrapping_add(CMD_HEADER_SIZE).wrapping_add(size) & wrap;
            if next == roff {
                self.ring_fault("client command ring does not advance");
                roff = woff;
                break;
            }
            roff = next;
        }
        self.wr(ram, ctl.wrapping_add(CCCB_READ_OFFSET), roff);
        self.irq = true;
    }

    /// Whether to trace the next GPU job.
    fn job_trace(&self) -> bool {
        self.trace && (self.trace_jobs == 0 || self.jobs < self.trace_jobs)
    }

    /// Read a 32-bit word at a firmware address.
    pub fn fw_read(&self, ram: &FlatRam, fw_addr: u32) -> u32 {
        self.rd(ram, fw_addr)
    }

    /// Whether an interrupt to the host is due (and clear the request).
    pub fn take_interrupt(&mut self) -> bool {
        std::mem::take(&mut self.irq)
    }
}

fn read32(ram: &FlatRam, pa: u64) -> Option<u32> {
    let o = pa.checked_sub(ram.base)? as usize;
    ram.mem.get(o..o.checked_add(4)?).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
}

fn read64(ram: &FlatRam, pa: u64) -> Option<u64> {
    let o = pa.checked_sub(ram.base)? as usize;
    ram.mem.get(o..o.checked_add(8)?).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RAM with the driver's MIPS page table mapping firmware pages 0 and 1
    /// (0xC000_0000..0xC000_1FFF) to physical 0x4000 and 0x5000, and a running firmware
    /// whose kernel ring control, commands, return slots and OS data sit in page 0.
    fn setup() -> (Firmware, FlatRam) {
        let mut ram = FlatRam::new(0, 0x10000);
        ram.load(0x1010, &0x2000u64.to_le_bytes()).unwrap();
        ram.load(0x2000, &((4u32 << 6) | 2).to_le_bytes()).unwrap();
        ram.load(0x2004, &((5u32 << 6) | 2).to_le_bytes()).unwrap();
        let fw = Firmware {
            running: true,
            boot_data: 0x1000,
            kccb_ctl: FW_BASE,
            kccb: FW_BASE + 0x200,
            kccb_rtn: FW_BASE + 0x800,
            osdata: FW_BASE + 0xc00,
            ..Default::default()
        };
        (fw, ram)
    }

    fn put(fw: &Firmware, ram: &mut FlatRam, fw_addr: u32, v: u32) {
        fw.wr(ram, fw_addr, v);
    }

    #[test]
    fn kernel_ring_with_zero_wrap_mask_does_not_hang() {
        let (mut fw, mut ram) = setup();
        // write offset 1, read offset 0, wrap mask 0: the read offset can never advance
        put(&fw, &mut ram, FW_BASE + CCB_WRITE_OFFSET, 1);
        put(&fw, &mut ram, FW_BASE + CCB_WRAP_MASK, 0);
        fw.doorbell(&mut ram);
        assert_eq!(fw.commands, 1);
        assert_eq!(fw.gpu_faults, 1);
    }

    #[test]
    fn client_command_with_a_bad_size_is_skipped() {
        let (mut fw, mut ram) = setup();
        put(&fw, &mut ram, FW_BASE + CCB_WRITE_OFFSET, 1);
        put(&fw, &mut ram, FW_BASE + CCB_WRAP_MASK, 0xf);
        // a kick of context 0xC000_0300: client ring at 0xC000_1000, control at 0xC000_0400
        let (cmd, ctx, ctl, ccb) = (FW_BASE + 0x200, FW_BASE + 0x300, FW_BASE + 0x400, FW_BASE + 0x1000);
        put(&fw, &mut ram, cmd, KCCB_CMD_KICK);
        put(&fw, &mut ram, cmd + 8, ctx);
        put(&fw, &mut ram, cmd + 12, 0x40);
        put(&fw, &mut ram, cmd + 16, 0xfff);
        put(&fw, &mut ram, ctx + CTX_CCBCTL, ctl);
        put(&fw, &mut ram, ctx + CTX_CCB, ccb);
        // an UPDATE claiming ~4 GiB of operands
        put(&fw, &mut ram, ccb, CCB_CMD_UPDATE);
        put(&fw, &mut ram, ccb + 4, 0xffff_fff0);
        fw.doorbell(&mut ram);
        assert_eq!(fw.gpu_faults, 1);
        // the ring is consumed up to the write offset
        assert_eq!(fw.rd(&ram, ctl + CCCB_READ_OFFSET), 0x40);
    }
}

emu_core::snap_struct!(Firmware {
    running, boot_data, kccb_ctl, kccb, kccb_rtn, osdata, sysdata, irq, boots, commands, jobs, invocations, gpu_faults, gfx,
} skip { trace, trace_jobs });
