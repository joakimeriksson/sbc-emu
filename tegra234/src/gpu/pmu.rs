//! GA10B's PMU as the silicon path of nvgpu sees it: a RISC-V falcon that the ACR (on the GSP)
//! prepared from the light-secure image in the WPR, booted by its ROM, running NVIDIA's PMU
//! RTOS. The RTOS itself (`gpmu_ucode_next_prod_image.bin`, signed) is not executed: this
//! models its host-visible protocol, as `common/pmu` in nvgpu speaks it.
//!
//! - Boot: nvgpu writes the boot arguments' address (+ the RISC-V EXTMEM2 window) to mailbox
//!   0/1 and starts the core. The ROM passes when the ACR has run, the core releases its priv
//!   lockdown, and the RTOS reads its command line: the super surface's GPU address and the
//!   PMU's instance block. It then posts the INIT message (`cmdmgmt_init`) in the super
//!   surface's message queue and raises SWGEN0.
//! - Commands: RPCs in the super surface's frame-buffer command queues (HPQ, LPQ). Writing a
//!   queue's head register submits elements; each RPC is executed here, its status (and output)
//!   written back in place, and a response with the command's sequence posted.
//! - Units: ACR (WPR init, bootstrapping FECS and GPCCS from the WPR), PERFMON (the GPU load:
//!   busy when a sample saw GPU work, approximated: engines complete work as it is submitted),
//!   PG and PG_LOADING (ELPG). ELPG is accepted and reported allowed, but graphics is never
//!   power-gated (the model has no idle filter); its statistics stay at zero.

use super::mmu;

const BASE: u64 = 0x10a000;
const IRQ_SET: u64 = BASE + 0x000;
const IRQ_CLEAR: u64 = BASE + 0x004;
const IRQ_STATUS: u64 = BASE + 0x008;
const MAILBOX0: u64 = BASE + 0x040;
const MAILBOX1: u64 = BASE + 0x044;
/// HWCFG2: debug mode (bit 3, production: 0), memory scrubbing (12), RISC-V priv lockdown (13).
const HWCFG2: u64 = BASE + 0x0f4;
const PRIV_LOCKDOWN: u32 = 1 << 13;
const ENGINE_RESET: u64 = BASE + 0x3c0;
const MSGQ_HEAD: u64 = BASE + 0x4c8;
const MSGQ_TAIL: u64 = BASE + 0x4cc;
/// Hardware mutexes shared by the driver and the RTOS: reading MUTEX_ID hands out a free owner
/// ID (1-254), writing it to a free mutex takes the mutex, writing 0 frees it, and writing the
/// ID to MUTEX_ID_RELEASE returns the ID.
const MUTEX_ID: u64 = BASE + 0x488;
const MUTEX_ID_RELEASE: u64 = BASE + 0x48c;
const MUTEX: u64 = BASE + 0x580;
const MUTEXES: u64 = 16;
const QUEUE_HEAD: u64 = BASE + 0x800;
const QUEUE_TAIL: u64 = BASE + 0x820;
const QUEUES: u64 = 8;
const RISCV_CPUCTL: u64 = 0x10b388;
const RISCV_STARTCPU: u32 = 1;
const RISCV_HALTED: u32 = 1 << 4;
const BROM_RETCODE: u64 = 0x10b65c;
const BROM_FAILED: u32 = 2;
const BROM_PASSED: u32 = 3;
const RISCV_IRQMASK: u64 = 0x10b528;
const RISCV_IRQDEST: u64 = 0x10b52c;
/// SWGEN0: a message is waiting (`pwr_falcon_irqstat_swgen0`).
const SWGEN0: u32 = 1 << 6;

/// The RISC-V core sees system memory through this window (`NV_NEXT_CORE_AMAP_EXTMEM2_START`).
const EXTMEM2: u64 = 0x8060_0000_0000_0000;
/// `nv_pmu_boot_params`: the boot loader and RTOS parameters (120 bytes), then
/// `pmu_cmdline_args_v7`, whose super surface descriptor is at 44 and the instance block at 60.
const ARGS: u64 = 120;
const ARGS_SUPER_SURFACE: u64 = ARGS + 44;
const ARGS_INSTANCE: u64 = ARGS + 60;

/// The super surface (`struct super_surface`): command queues, then the message queue.
const SS_CMD_QUEUES: u64 = 512;
const CMD_ELEMENT: u64 = 2048;
const CMD_ELEMENTS: u32 = 16;
const SS_MSG_QUEUE: u64 = 66048;
const MSG_ELEMENT: u64 = 64;
const MSG_ELEMENTS: u32 = 16;
/// An element starts with its queue header: 8 bytes in a command (element index, heap size and
/// offset), 4 in a message.
const CMD_HEADER: u64 = 8;
const MSG_HEADER: u64 = 4;

const UNIT_CMDMGMT: u8 = 0x01;
const UNIT_PG: u8 = 0x03;
const UNIT_PG_LOADING: u8 = 0x06;
const UNIT_ACR: u8 = 0x0a;
const UNIT_PERFMON_T18X: u8 = 0x11;
const UNIT_PERFMON: u8 = 0x12;
const RPC_CMD: u8 = 0x80;
const FLAGS_RPC_EVENT: u8 = 1 << 3;

const ACR_BOOTSTRAP_FALCON: u8 = 0x02;
const ACR_BOOTSTRAP_GR_FALCONS: u8 = 0x03;
const PERFMON_INIT: u8 = 0x00;
const PERFMON_QUERY: u8 = 0x04;
const PERFMON_INIT_EVENT: u8 = 0x01;
const PG_DISALLOW: u8 = 0x05;
const PG_ASYNC_CMD_RESP: u8 = 0x00;
const PG_MSG_ASYNC_CMD_DISALLOW: u8 = 0x00;

/// Queue placement the RTOS reports: physical queue IDs (HPQ, LPQ, -, message) and its heap.
const HEAP_OFFSET: u16 = 0x8000;
const HEAP_SIZE: u16 = 0x6000;

/// What a command asked of the rest of the GPU.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Effect {
    /// The ACR unit bootstrapped these light-secure falcons (a mask of falcon IDs) from the WPR.
    BootstrapFalcons(u32),
}

#[derive(Default)]
pub(super) struct Pmu {
    irq_status: u32,
    irq_mask: u32,
    irq_dest: u32,
    mailbox: [u32; 2],
    running: bool,
    brom_status: u32,
    /// The INIT message is due when the driver polls for the priv lockdown's release.
    init_pending: bool,
    super_surface: u64,
    instance: u64,
    queue_head: [u32; QUEUES as usize],
    queue_tail: [u32; QUEUES as usize],
    msgq_head: u32,
    msgq_tail: u32,
    mutex: [u8; MUTEXES as usize],
    /// Owner IDs handed out (bit per ID).
    mutex_ids: [u64; 4],
    /// Messages that did not fit in the message queue yet.
    backlog: std::collections::VecDeque<Vec<u8>>,
    /// GPU work submitted so far (the host's submission count), set by the GPU before each
    /// register write; PERFMON's load compares it with the previous query's.
    pub work: u64,
    work_at_query: u64,
    pub perfmon_queries: u64,
}

impl Pmu {
    pub fn irq(&self) -> bool {
        self.irq_status & self.irq_mask & self.irq_dest != 0
    }

    pub fn read(&mut self, offset: u64, ram: &mut [u8]) -> Option<u32> {
        Some(match offset {
            IRQ_STATUS => self.irq_status,
            MAILBOX0 => self.mailbox[0],
            MAILBOX1 => self.mailbox[1],
            HWCFG2 => {
                if self.init_pending {
                    // The RTOS is up: it releases the lockdown and posts its INIT message.
                    self.init_pending = false;
                    self.post_init(ram);
                }
                u32::from(self.brom_status == BROM_PASSED && !self.running) * PRIV_LOCKDOWN
            }
            RISCV_CPUCTL => u32::from(!self.running && self.brom_status != 0) * RISCV_HALTED,
            BROM_RETCODE => self.brom_status,
            RISCV_IRQMASK => self.irq_mask,
            RISCV_IRQDEST => self.irq_dest,
            MSGQ_HEAD => self.msgq_head,
            MSGQ_TAIL => self.msgq_tail,
            MUTEX_ID => {
                match (1..255u32).find(|&id| self.mutex_ids[id as usize / 64] & 1 << (id % 64) == 0) {
                    Some(id) => {
                        self.mutex_ids[id as usize / 64] |= 1 << (id % 64);
                        id
                    }
                    None => 0xff, // none available
                }
            }
            MUTEX_ID_RELEASE => 0,
            o if (MUTEX..MUTEX + 4 * MUTEXES).contains(&o) => u32::from(self.mutex[((o - MUTEX) / 4) as usize]),
            o if (QUEUE_HEAD..QUEUE_HEAD + 4 * QUEUES).contains(&o) => {
                self.queue_head[((o - QUEUE_HEAD) / 4) as usize]
            }
            o if (QUEUE_TAIL..QUEUE_TAIL + 4 * QUEUES).contains(&o) => {
                self.queue_tail[((o - QUEUE_TAIL) / 4) as usize]
            }
            _ => return None,
        })
    }

    /// A register write; `acr_done` says whether the ACR has prepared the PMU's image.
    pub fn write(
        &mut self,
        offset: u64,
        value: u32,
        ram: &mut [u8],
        acr_done: bool,
        effects: &mut Vec<Effect>,
    ) -> bool {
        match offset {
            IRQ_SET => self.irq_status |= value & SWGEN0,
            IRQ_CLEAR => self.irq_status &= !value,
            IRQ_STATUS | HWCFG2 | BROM_RETCODE | MSGQ_HEAD => {}
            MAILBOX0 => self.mailbox[0] = value,
            MAILBOX1 => self.mailbox[1] = value,
            RISCV_IRQMASK => self.irq_mask = value,
            RISCV_IRQDEST => self.irq_dest = value,
            MUTEX_ID => {}
            MUTEX_ID_RELEASE => {
                let id = value & 0xff;
                if (1..255).contains(&id) {
                    self.mutex_ids[id as usize / 64] &= !(1 << (id % 64));
                }
            }
            o if (MUTEX..MUTEX + 4 * MUTEXES).contains(&o) => {
                let mutex = &mut self.mutex[((o - MUTEX) / 4) as usize];
                let id = (value & 0xff) as u8;
                if id == 0 {
                    *mutex = 0;
                } else if *mutex == 0 {
                    *mutex = id;
                }
            }
            ENGINE_RESET => {
                if value & 1 != 0 {
                    *self = Self { work: self.work, perfmon_queries: self.perfmon_queries, ..Self::default() };
                }
                return false;
            }
            RISCV_CPUCTL => {
                if value & RISCV_STARTCPU != 0 && !self.running {
                    self.boot(ram, acr_done);
                }
            }
            MSGQ_TAIL => {
                self.msgq_tail = value % MSG_ELEMENTS;
                self.flush_backlog(ram);
            }
            o if (QUEUE_HEAD..QUEUE_HEAD + 4 * QUEUES).contains(&o) => {
                let queue = ((o - QUEUE_HEAD) / 4) as usize;
                self.queue_head[queue] = value % CMD_ELEMENTS;
                if self.running {
                    self.process(queue, ram, effects);
                }
            }
            o if (QUEUE_TAIL..QUEUE_TAIL + 4 * QUEUES).contains(&o) => {}
            _ => return false,
        }
        true
    }

    /// The ROM verifies and starts the image the ACR placed; the RTOS reads its boot arguments.
    fn boot(&mut self, ram: &mut [u8], acr_done: bool) {
        let args = (u64::from(self.mailbox[0]) | u64::from(self.mailbox[1]) << 32).wrapping_sub(EXTMEM2);
        let word = |offset: u64| mmu::word(ram, args + offset);
        let (Some(ss_lo), Some(ss_hi), Some(bind)) =
            (word(ARGS_SUPER_SURFACE), word(ARGS_SUPER_SURFACE + 4), word(ARGS_INSTANCE))
        else {
            self.brom_status = BROM_FAILED;
            return;
        };
        if !acr_done {
            self.brom_status = BROM_FAILED;
            return;
        }
        self.brom_status = BROM_PASSED;
        self.running = true;
        self.super_surface = u64::from(ss_lo) | u64::from(ss_hi) << 32;
        self.instance = u64::from(bind & 0x0fff_ffff) << 12;
        self.queue_head = [0; QUEUES as usize];
        self.queue_tail = [0; QUEUES as usize];
        self.msgq_head = 0;
        self.msgq_tail = 0;
        self.backlog.clear();
        self.init_pending = true;
        if trace() {
            eprintln!("[pmu] boot: super surface {:#x} instance {:#x}", self.super_surface, self.instance);
        }
    }

    fn post_init(&mut self, ram: &mut [u8]) {
        // The RTOS configures its own interrupt routing (`ga10b_pmu_enable_irq`).
        self.irq_mask = SWGEN0;
        self.irq_dest = SWGEN0;
        // cmdmgmt_init: RPC header (unit, function 0: PMU_INIT), status, the queues' physical
        // IDs and sizes, the queues' start, the driver-managed heap, no debug entry point.
        let mut body = [0u8; 36];
        body[0] = UNIT_CMDMGMT;
        body[13..17].copy_from_slice(&[0, 1, 2, 3]);
        for (i, size) in [CMD_ELEMENTS, CMD_ELEMENTS, 0, MSG_ELEMENTS].into_iter().enumerate() {
            body[18 + 2 * i..20 + 2 * i].copy_from_slice(&(size as u16).to_le_bytes());
        }
        body[28..30].copy_from_slice(&HEAP_OFFSET.to_le_bytes());
        body[30..32].copy_from_slice(&HEAP_SIZE.to_le_bytes());
        body[32..34].copy_from_slice(&0xffffu16.to_le_bytes());
        self.post(ram, UNIT_CMDMGMT, 0, 0, &body);
    }

    /// Queue a message (`pmu_hdr` + body) in the super surface's message queue.
    fn post(&mut self, ram: &mut [u8], unit: u8, flags: u8, sequence: u8, body: &[u8]) {
        let mut message = Vec::with_capacity(4 + body.len());
        message.extend_from_slice(&[unit, (4 + body.len()) as u8, flags, sequence]);
        message.extend_from_slice(body);
        self.backlog.push_back(message);
        self.flush_backlog(ram);
    }

    fn flush_backlog(&mut self, ram: &mut [u8]) {
        while let Some(message) = self.backlog.front() {
            let next = (self.msgq_head + 1) % MSG_ELEMENTS;
            if next == self.msgq_tail {
                return; // full: the driver frees elements by advancing the tail
            }
            let element = self.super_surface + SS_MSG_QUEUE + u64::from(self.msgq_head) * MSG_ELEMENT;
            let mut bytes = [0u8; MSG_ELEMENT as usize];
            let n = message.len().min(bytes.len() - MSG_HEADER as usize);
            bytes[MSG_HEADER as usize..MSG_HEADER as usize + n].copy_from_slice(&message[..n]);
            let Some(mut view) = mmu::View::from_instance(ram, self.instance) else { return };
            if view.write(element, &bytes).is_none() {
                if trace() {
                    eprintln!("[pmu] message queue element {element:#x} is not mapped");
                }
                return;
            }
            if trace() {
                eprintln!("[pmu] message {:02x?} at {}", &message[..4], self.msgq_head);
            }
            self.backlog.pop_front();
            self.msgq_head = next;
            self.irq_status |= SWGEN0;
        }
    }

    /// Execute the RPCs between the queue's tail and its new head.
    fn process(&mut self, queue: usize, ram: &mut [u8], effects: &mut Vec<Effect>) {
        while self.queue_tail[queue] != self.queue_head[queue] {
            let position = self.queue_tail[queue];
            let element =
                self.super_surface + SS_CMD_QUEUES + (queue as u64 * u64::from(CMD_ELEMENTS) + u64::from(position)) * CMD_ELEMENT;
            let mut bytes = vec![0u8; CMD_ELEMENT as usize];
            let Some(view) = mmu::View::from_instance(ram, self.instance) else { return };
            if view.read(element, &mut bytes).is_none() {
                return;
            }
            self.queue_tail[queue] = (position + 1) % CMD_ELEMENTS;
            let heap_offset = u16::from_le_bytes([bytes[6], bytes[7]]);
            let cmd = &bytes[CMD_HEADER as usize..];
            let [unit, _size, _flags, sequence] = [cmd[0], cmd[1], cmd[2], cmd[3]];
            if cmd[4] != RPC_CMD {
                continue; // nvgpu's FB queues carry RPCs only
            }
            let rpc_size = usize::from(u16::from_le_bytes([cmd[6], cmd[7]]));
            let rpc_ptr = u32::from_le_bytes([cmd[8], cmd[9], cmd[10], cmd[11]]);
            let start = (rpc_ptr.wrapping_sub(u32::from(heap_offset))) as usize;
            if rpc_size < 12 || start + rpc_size > bytes.len() {
                continue;
            }
            let rpc = &mut bytes[start..start + rpc_size];
            let function = rpc[1];
            if trace() {
                eprintln!("[pmu] queue {queue} rpc unit {unit:#x} function {function:#x} seq {sequence}");
            }
            rpc[3] = 0; // flcn_status: FLCN_OK
            let event = self.execute(unit, function, rpc, effects);
            let Some(mut view) = mmu::View::from_instance(ram, self.instance) else { return };
            if view.write(element + start as u64, &bytes[start..start + rpc_size]).is_none() {
                return;
            }
            // The response: the unit's RPC header (unit, function, transfer time).
            let mut header = [0u8; 12];
            header[..2].copy_from_slice(&[unit, function]);
            self.post(ram, unit, 0, sequence, &header);
            if let Some((unit, body)) = event {
                self.post(ram, unit, FLAGS_RPC_EVENT, 0, &body);
            }
        }
    }

    /// One RPC's work; returns an event to send after its response.
    fn execute(&mut self, unit: u8, function: u8, rpc: &mut [u8], effects: &mut Vec<Effect>) -> Option<(u8, Vec<u8>)> {
        let u32_at = |rpc: &[u8], at: usize| rpc.get(at..at + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
        match (unit, function) {
            (UNIT_ACR, ACR_BOOTSTRAP_GR_FALCONS) => effects.push(Effect::BootstrapFalcons(u32_at(rpc, 12))),
            (UNIT_ACR, ACR_BOOTSTRAP_FALCON) => effects.push(Effect::BootstrapFalcons(1 << u32_at(rpc, 12))),
            (UNIT_PERFMON_T18X | UNIT_PERFMON, PERFMON_INIT) => {
                // The RTOS confirms its counters are set up with an INIT event too.
                let mut body = vec![0u8; 12];
                body[..2].copy_from_slice(&[unit, PERFMON_INIT_EVENT]);
                return Some((unit, body));
            }
            (UNIT_PERFMON_T18X | UNIT_PERFMON, PERFMON_QUERY) => {
                // The busy fraction of the GR/CE counter since the last sample, in 0.01 %. The
                // engines run each submission to completion when it is rung, so there is no
                // busy time to count: a sample with work in it reads fully busy, else idle.
                self.perfmon_queries += 1;
                let load: u16 = if self.work != self.work_at_query { 10000 } else { 0 };
                self.work_at_query = self.work;
                rpc[12..14].copy_from_slice(&load.to_le_bytes());
            }
            (UNIT_PG, PG_DISALLOW) => {
                // ELPG for this controller is now disallowed: the PG task says so asynchronously.
                let mut body = vec![0u8; 16];
                body[..2].copy_from_slice(&[UNIT_PG, PG_ASYNC_CMD_RESP]);
                body[12] = rpc[12];
                body[13] = PG_MSG_ASYNC_CMD_DISALLOW;
                return Some((UNIT_PG, body));
            }
            (UNIT_PG | UNIT_PG_LOADING, _) | (UNIT_ACR, _) | (UNIT_PERFMON_T18X | UNIT_PERFMON, _) => {}
            _ => {}
        }
        None
    }
}

fn trace() -> bool {
    static TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *TRACE.get_or_init(|| std::env::var_os("SBC_TRACE_PMU").is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RAM_BASE;

    const INSTANCE: u64 = RAM_BASE + 0x1000;
    const ROOT: u64 = RAM_BASE + 0x2000;
    /// The super surface's GPU address and where it lives.
    const SS_VA: u64 = 0x10_0000;
    const SS_PA: u64 = RAM_BASE + 0x20_0000;
    const ARGS_PA: u64 = RAM_BASE + 0x8000;

    fn put(ram: &mut [u8], address: u64, bytes: &[u8]) {
        let at = (address - RAM_BASE) as usize;
        ram[at..at + bytes.len()].copy_from_slice(bytes);
    }

    fn get(ram: &[u8], address: u64, len: usize) -> Vec<u8> {
        let at = (address - RAM_BASE) as usize;
        ram[at..at + len].to_vec()
    }

    /// RAM with the PMU's instance block mapping SS_VA..+1 MiB to SS_PA in 4 KiB pages, and the
    /// boot arguments.
    fn setup() -> Vec<u8> {
        let mut ram = vec![0u8; 4 << 20];
        put(&mut ram, INSTANCE + 0x200, &ROOT.to_le_bytes());
        for (entry, next) in [(ROOT, ROOT + 0x1000), (ROOT + 0x1000, ROOT + 0x2000), (ROOT + 0x2000, ROOT + 0x3000)] {
            put(&mut ram, entry, &((next >> 4) | 4).to_le_bytes());
        }
        let small = ROOT + 0x4000;
        put(&mut ram, ROOT + 0x3008, &((small >> 4) | 4).to_le_bytes());
        for page in 0..256u64 {
            let pte = ((SS_PA + page * 0x1000) >> 4) | 5;
            put(&mut ram, small + ((SS_VA >> 12) + page) * 8, &pte.to_le_bytes());
        }
        put(&mut ram, ARGS_PA + ARGS_SUPER_SURFACE, &(SS_VA as u32).to_le_bytes());
        put(&mut ram, ARGS_PA + ARGS_INSTANCE, &((INSTANCE >> 12) as u32 | 1 << 30).to_le_bytes());
        ram
    }

    fn boot(pmu: &mut Pmu, ram: &mut [u8], acr_done: bool) {
        let args = EXTMEM2 + ARGS_PA;
        let mut effects = Vec::new();
        pmu.write(MAILBOX0, args as u32, ram, acr_done, &mut effects);
        pmu.write(MAILBOX1, (args >> 32) as u32, ram, acr_done, &mut effects);
        pmu.write(RISCV_CPUCTL, RISCV_STARTCPU, ram, acr_done, &mut effects);
    }

    #[test]
    fn rom_fails_without_the_acr() {
        let mut ram = setup();
        let mut pmu = Pmu::default();
        boot(&mut pmu, &mut ram, false);
        assert_eq!(pmu.read(BROM_RETCODE, &mut ram), Some(BROM_FAILED));
        pmu.read(HWCFG2, &mut ram);
        assert!(!pmu.irq());
    }

    #[test]
    fn rtos_posts_init_and_answers_an_rpc() {
        let mut ram = setup();
        let mut pmu = Pmu::default();
        boot(&mut pmu, &mut ram, true);
        assert_eq!(pmu.read(BROM_RETCODE, &mut ram), Some(BROM_PASSED));
        assert!(!pmu.irq());
        // The driver polls for the lockdown's release; the RTOS then posts INIT.
        assert_eq!(pmu.read(HWCFG2, &mut ram).unwrap() & PRIV_LOCKDOWN, 0);
        assert!(pmu.irq());
        assert_eq!(pmu.read(MSGQ_HEAD, &mut ram), Some(1));
        let init = get(&ram, SS_PA + SS_MSG_QUEUE + MSG_HEADER, 40);
        assert_eq!(&init[..4], &[UNIT_CMDMGMT, 40, 0, 0]);
        assert_eq!(&init[4 + 13..4 + 17], &[0, 1, 2, 3]);
        assert_eq!(u16::from_le_bytes([init[4 + 28], init[4 + 29]]), HEAP_OFFSET);

        let mut effects = Vec::new();
        pmu.write(IRQ_CLEAR, SWGEN0, &mut ram, true, &mut effects);
        pmu.write(MSGQ_TAIL, 1, &mut ram, true, &mut effects);
        // An ACR BOOTSTRAP_GR_FALCONS RPC in LPQ element 0: FBQ header, command, RPC.
        let element = SS_PA + SS_CMD_QUEUES + u64::from(CMD_ELEMENTS) * CMD_ELEMENT;
        let heap = u32::from(HEAP_OFFSET);
        let mut command = vec![0u8; 8 + 12 + 24];
        command[6..8].copy_from_slice(&HEAP_OFFSET.to_le_bytes());
        command[8..12].copy_from_slice(&[UNIT_ACR, 12, 3, 7]);
        command[12] = RPC_CMD;
        command[14..16].copy_from_slice(&24u16.to_le_bytes());
        command[16..20].copy_from_slice(&(heap + 20).to_le_bytes());
        command[20..24].copy_from_slice(&[UNIT_ACR, ACR_BOOTSTRAP_GR_FALCONS, 0, 0xff]);
        command[32..36].copy_from_slice(&0xcu32.to_le_bytes());
        put(&mut ram, element, &command);
        pmu.write(QUEUE_HEAD + 4, 1, &mut ram, true, &mut effects);
        assert_eq!(effects, [Effect::BootstrapFalcons(0xc)]);
        assert_eq!(get(&ram, element + 20 + 3, 1), [0]); // flcn_status: OK, written back
        assert!(pmu.irq());
        assert_eq!(pmu.read(MSGQ_HEAD, &mut ram), Some(2));
        let reply = get(&ram, SS_PA + SS_MSG_QUEUE + MSG_ELEMENT + MSG_HEADER, 6);
        assert_eq!(reply, [UNIT_ACR, 16, 0, 7, UNIT_ACR, ACR_BOOTSTRAP_GR_FALCONS]);
    }

    #[test]
    fn hardware_mutexes_hand_out_ids_and_exclude() {
        let mut ram = Vec::new();
        let mut pmu = Pmu::default();
        let mut effects = Vec::new();
        let a = pmu.read(MUTEX_ID, &mut ram).unwrap();
        let b = pmu.read(MUTEX_ID, &mut ram).unwrap();
        assert!(a != b && a != 0 && b != 0);
        pmu.write(MUTEX + 8, a, &mut ram, false, &mut effects);
        pmu.write(MUTEX + 8, b, &mut ram, false, &mut effects);
        assert_eq!(pmu.read(MUTEX + 8, &mut ram), Some(a));
        pmu.write(MUTEX + 8, 0, &mut ram, false, &mut effects);
        pmu.write(MUTEX + 8, b, &mut ram, false, &mut effects);
        assert_eq!(pmu.read(MUTEX + 8, &mut ram), Some(b));
        pmu.write(MUTEX_ID_RELEASE, a, &mut ram, false, &mut effects);
        assert_eq!(pmu.read(MUTEX_ID, &mut ram), Some(a));
    }
}
