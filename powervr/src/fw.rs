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
        let pt_page = read64(ram, self.boot_data + 16 + 8 * (idx / 1024))?;
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
        self.wr(ram, self.sysdata + SYSDATA_POW_STATE, POW_IDLE);
        self.running = true;
        self.boots += 1;
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
    pub fn doorbell(&mut self, ram: &mut FlatRam) {
        if !self.running {
            return;
        }
        let mut any = false;
        loop {
            let (wr, rd) = (self.rd(ram, self.kccb_ctl + CCB_WRITE_OFFSET), self.rd(ram, self.kccb_ctl + CCB_READ_OFFSET));
            if wr == rd || !self.running {
                break;
            }
            let wrap = self.rd(ram, self.kccb_ctl + CCB_WRAP_MASK);
            let cmd = self.kccb + rd * KCCB_CMD_SIZE;
            let rtn = self.command(ram, cmd);
            self.wr(ram, self.kccb_rtn + 4 * rd, rtn);
            self.wr(ram, self.kccb_ctl + CCB_READ_OFFSET, (rd + 1) & wrap);
            let n = self.rd(ram, self.osdata + OSDATA_KCCB_CMDS_EXECUTED);
            self.wr(ram, self.osdata + OSDATA_KCCB_CMDS_EXECUTED, n.wrapping_add(1));
            self.commands += 1;
            any = true;
        }
        if any {
            let n = self.rd(ram, self.osdata + OSDATA_INTERRUPT_COUNT);
            self.wr(ram, self.osdata + OSDATA_INTERRUPT_COUNT, n.wrapping_add(1));
            self.irq = true;
        }
    }

    /// One kernel command; returns its return-slot flags.
    fn command(&mut self, ram: &mut FlatRam, cmd: u32) -> u32 {
        let ty = self.rd(ram, cmd);
        if self.trace {
            eprintln!("[gpu fw] kccb command {ty:#x}");
        }
        match ty {
            KCCB_CMD_POW => {
                let state = match self.rd(ram, cmd + 8) {
                    1 => POW_OFF,
                    2 => POW_FORCED_IDLE,
                    _ => POW_IDLE,
                };
                self.wr(ram, self.sysdata + SYSDATA_POW_STATE, state);
                let sync = self.rd(ram, self.osdata + OSDATA_POWER_SYNC);
                self.wr(ram, sync, 1);
                if state == POW_OFF {
                    self.running = false;
                }
            }
            KCCB_CMD_KICK => self.kick(ram, cmd + 8),
            KCCB_CMD_COMBINED_GEOM_FRAG_KICK => {
                self.kick(ram, cmd + 8);
                self.kick(ram, cmd + 40);
            }
            KCCB_CMD_MMUCACHE => {
                // nothing is cached: translations are read from memory each time
                let (sync, value) = (self.rd(ram, cmd + 12), self.rd(ram, cmd + 16));
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
    fn kick(&mut self, ram: &mut FlatRam, kick: u32) {
        let ctx = self.rd(ram, kick);
        let (ctl, ccb, mem_ctx) = (self.rd(ram, ctx + CTX_CCBCTL), self.rd(ram, ctx + CTX_CCB), self.rd(ram, ctx + CTX_MEM_CONTEXT));
        // the page catalogue of the context's GPU virtual address space
        let pc = self.translate(ram, mem_ctx).and_then(|pa| read64(ram, pa)).unwrap_or(0);
        // the kick carries the new write offset and wrap mask; the firmware keeps them in the
        // control block
        let (woff, wrap) = (self.rd(ram, kick + 4), self.rd(ram, kick + 8));
        self.wr(ram, ctl + CCCB_WRITE_OFFSET, woff);
        self.wr(ram, ctl + CCCB_WRAP_MASK, wrap);
        let mut roff = self.rd(ram, ctl + CCCB_READ_OFFSET);
        let mut guard = 0;
        while roff != woff && guard < 4096 {
            guard += 1;
            let hdr = ccb + roff;
            let (ty, size) = (self.rd(ram, hdr), self.rd(ram, hdr + 4));
            let body = hdr + CMD_HEADER_SIZE;
            if self.trace {
                eprintln!("[gpu fw] ctx {ctx:#x} ccb cmd {ty:#x} size {size}");
            }
            match ty {
                // all earlier work is complete when a command is reached: fences are met
                CCB_CMD_FENCE | CCB_CMD_FENCE_PR => {}
                CCB_CMD_UPDATE | CCB_CMD_UNFENCED_UPDATE => {
                    for i in 0..size / 8 {
                        let (addr, value) = (self.rd(ram, body + 8 * i), self.rd(ram, body + 8 * i + 4));
                        self.wr(ram, addr, value);
                    }
                }
                CCB_CMD_CDM => {
                    let mut vm = crate::vm::Vm::new(pc);
                    crate::cdm::run(self, ram, &mut vm, body);
                    self.jobs += 1;
                }
                // a partial render: only needed when the parameter buffer runs out, which it
                // never does here; its fences and updates are the commands around it
                CCB_CMD_FRAG_PR => {
                    self.jobs += 1;
                }
                CCB_CMD_GEOM | CCB_CMD_FRAG | CCB_CMD_TQ_3D => {
                    let words: Vec<u32> = (0..size / 4).map(|i| self.rd(ram, body + 4 * i)).collect();
                    // the render size: the HWRT data's common part, screen_pixel_max
                    let screen = match ty {
                        CCB_CMD_FRAG => {
                            let common = self.rd(ram, words[1] + 164);
                            self.rd(ram, common + 4)
                        }
                        _ => 0,
                    };
                    let mut vm = crate::vm::Vm::new(pc);
                    let mut mem = crate::cdm::GpuMem { ram, vm: &mut vm };
                    let trace = self.trace;
                    let r = match ty {
                        CCB_CMD_GEOM => self.gfx.geom(&mut mem, &words, trace),
                        CCB_CMD_FRAG => self.gfx.frag(&mut mem, &words, screen, trace),
                        _ => self.gfx.transfer(&mut mem, &words, trace),
                    };
                    if let Err(e) = r {
                        if trace || self.gpu_faults == 0 {
                            eprintln!("[gpu gfx] job {ty:#x}: {e}");
                        }
                        self.gpu_faults += 1;
                    }
                    self.jobs += 1;
                }
                _ => {}
            }
            roff = (roff + CMD_HEADER_SIZE + size) & wrap;
        }
        self.wr(ram, ctl + CCCB_READ_OFFSET, roff);
        self.irq = true;
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
    ram.mem.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
}

fn read64(ram: &FlatRam, pa: u64) -> Option<u64> {
    let o = pa.checked_sub(ram.base)? as usize;
    ram.mem.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
}
