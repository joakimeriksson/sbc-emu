//! Arm PrimeCell DMA-330 (PL330) DMA controller, as Linux's `pl330` driver uses it: the
//! channel threads run the microcode the driver writes to memory (DMAMOV, DMALP/DMALPEND,
//! DMAWFP, DMALD/DMAST and their peripheral forms, DMASEV, DMAEND...), started through the
//! debug interface (DMAGO in DBGINST0/1, then DBGCMD). Events with their interrupt enabled
//! raise the interrupt line; the driver reads progress from the channels' SAR/DAR.
//!
//! The controller is modelled as fast: a thread runs until it ends, waits for a peripheral
//! request (DMAWFP with the request low), or faults. The board raises a peripheral's
//! request through [`Pl330Bus::request`] and runs the controller again when it rises.

/// What the controller reaches: memory and peripheral FIFOs, and the request lines.
pub trait Pl330Bus {
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> bool;
    fn write(&mut self, addr: u32, data: &[u8]) -> bool;
    /// Peripheral `peri`'s DMA request is active.
    fn request(&mut self, peri: u8) -> bool;
}

const CHANNELS: usize = 8;
const EVENTS: usize = 16;
const PERIPHERALS: u32 = 32;
/// Instructions a thread runs per call before yielding (a runaway loop guard).
const STEP_BUDGET: usize = 1 << 20;

// Registers
const DS: u64 = 0x000;
const DPC: u64 = 0x004;
const INTEN: u64 = 0x020;
const ES: u64 = 0x024;
const INTSTATUS: u64 = 0x028;
const INTCLR: u64 = 0x02c;
const FSM: u64 = 0x030;
const FSC: u64 = 0x034;
const FTM: u64 = 0x038;
const FTC: u64 = 0x040;
const CS: u64 = 0x100;
const SA: u64 = 0x400;
const DBGSTATUS: u64 = 0xd00;
const DBGCMD: u64 = 0xd04;
const DBGINST0: u64 = 0xd08;
const DBGINST1: u64 = 0xd0c;
const CR0: u64 = 0xe00;
const CR1: u64 = 0xe04;
const CR2: u64 = 0xe08;
const CR3: u64 = 0xe0c;
const CR4: u64 = 0xe10;
const CRD: u64 = 0xe14;
const PERIPH_ID: u64 = 0xfe0;
const PCELL_ID: u64 = 0xff0;

/// Peripheral ID: part 0x330, designer 0x41 (Arm), revision r1p1; then the PrimeCell ID.
const PERIPH_ID_BYTES: [u32; 4] = [0x30, 0x13, 0x24, 0x00];
const PCELL_ID_BYTES: [u32; 4] = [0x0d, 0xf0, 0x05, 0xb1];

// Thread states (DS / CS low nibble)
const ST_STOP: u32 = 0x0;
const ST_EXEC: u32 = 0x1;
const ST_WFP: u32 = 0x7;
const ST_FAULT: u32 = 0xf;

// Fault types (FTC)
const FT_UNDEF_INSTR: u32 = 1 << 0;
const FT_OPERAND_INVALID: u32 = 1 << 1;
const FT_DATA_READ: u32 = 1 << 18;
const FT_DATA_WRITE: u32 = 1 << 19;

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
enum State {
    #[default]
    Stopped,
    Executing,
    /// Waiting for a peripheral request: the peripheral and whether the wait was DMAWFPB.
    Wfp(u8),
    Fault,
}

#[derive(Clone, Default)]
struct Channel {
    state: State,
    pc: u32,
    sar: u32,
    dar: u32,
    ccr: u32,
    lc: [u32; 2],
    /// The request type of the last DMAWFP: burst (true) or single.
    burst: bool,
    /// Data loaded and not yet stored (the MFIFO).
    fifo: std::collections::VecDeque<u8>,
    fault: u32,
}

#[derive(Default)]
pub struct Pl330 {
    channels: [Channel; CHANNELS],
    inten: u32,
    /// Event status: events signalled (with their interrupt enabled, the interrupt).
    es: u32,
    dbginst: [u32; 2],
    manager_fault: u32,
    pub log: bool,
}

impl Pl330 {
    pub fn new() -> Self {
        Self { log: std::env::var_os("SBC_PL330_LOG").is_some(), ..Default::default() }
    }

    /// The controller's interrupt (one line for all events, as Linux's handler serves all).
    pub fn irq(&self) -> bool {
        self.es & self.inten != 0
    }

    /// A thread waits on peripheral `peri`.
    pub fn waiting_on(&self, peri: u8) -> bool {
        self.channels.iter().any(|c| c.state == State::Wfp(peri))
    }

    /// Some thread waits for a peripheral request.
    pub fn waiting(&self) -> bool {
        self.channels.iter().any(|c| matches!(c.state, State::Wfp(_)))
    }

    /// A register read. The 4 KiB register file repeats over a larger region (Linux's AMBA
    /// bus reads the IDs from the region's last 32 bytes).
    pub fn read(&self, offset: u64) -> u32 {
        match offset & 0xfff {
            DS | DPC | FSM | FTM => 0,
            INTEN => self.inten,
            ES => self.es,
            INTSTATUS => self.es & self.inten,
            FSC => self.channels.iter().enumerate().filter(|(_, c)| c.state == State::Fault).fold(0, |m, (i, _)| m | 1 << i),
            o if (FTC..FTC + 4 * CHANNELS as u64).contains(&o) => self.channels[((o - FTC) / 4) as usize].fault,
            o if (CS..CS + 8 * CHANNELS as u64).contains(&o) => {
                let c = &self.channels[((o - CS) / 8) as usize];
                if o & 4 == 0 {
                    match c.state {
                        State::Stopped => ST_STOP,
                        State::Executing => ST_EXEC,
                        State::Wfp(p) => ST_WFP | (p as u32) << 4,
                        State::Fault => ST_FAULT,
                    }
                } else {
                    c.pc
                }
            }
            o if (SA..SA + 0x20 * CHANNELS as u64).contains(&o) => {
                let c = &self.channels[((o - SA) / 0x20) as usize];
                match (o - SA) % 0x20 {
                    0x00 => c.sar,
                    0x04 => c.dar,
                    0x08 => c.ccr,
                    0x0c => c.lc[0],
                    0x10 => c.lc[1],
                    _ => 0,
                }
            }
            DBGSTATUS => 0, // never busy: debug instructions run at once
            DBGINST0 => self.dbginst[0],
            DBGINST1 => self.dbginst[1],
            // CR0: peripheral requests, 8 channels, 32 peripherals, 16 events
            CR0 => 1 | ((CHANNELS as u32 - 1) << 4) | ((PERIPHERALS - 1) << 12) | ((EVENTS as u32 - 1) << 17),
            CR1 => 0x47, // 8 instruction cache lines of 16 words
            CR2 | CR3 | CR4 => 0,
            // CRD: 64-bit AXI, 8-deep read/write queues, a 128-line data buffer
            CRD => 3 | 7 << 4 | 7 << 8 | 7 << 12 | 7 << 16 | 127 << 20,
            o if (PERIPH_ID..PERIPH_ID + 16).contains(&o) => PERIPH_ID_BYTES[((o - PERIPH_ID) / 4) as usize],
            o if (PCELL_ID..PCELL_ID + 16).contains(&o) => PCELL_ID_BYTES[((o - PCELL_ID) / 4) as usize],
            _ => 0,
        }
    }

    pub fn write(&mut self, offset: u64, value: u32, bus: &mut dyn Pl330Bus) {
        match offset & 0xfff {
            INTEN => self.inten = value,
            INTCLR => self.es &= !value,
            DBGINST0 => self.dbginst[0] = value,
            DBGINST1 => self.dbginst[1] = value,
            DBGCMD if value & 3 == 0 => {
                self.debug_instruction();
                self.run(bus);
            }
            _ => {}
        }
    }

    /// DBGINST0/1: one instruction for the manager thread (or a channel thread, bit 0).
    fn debug_instruction(&mut self) {
        let [i0, i1] = self.dbginst;
        let insn = [(i0 >> 16) as u8, (i0 >> 24) as u8, i1 as u8, (i1 >> 8) as u8, (i1 >> 16) as u8, (i1 >> 24) as u8];
        let channel = (i0 & 1 != 0).then_some(((i0 >> 8) & 7) as usize);
        if self.log {
            eprintln!("[pl330] debug {insn:02x?} on {}", channel.map_or("manager".into(), |c| format!("channel {c}")));
        }
        match (channel, insn[0]) {
            // DMAKILL
            (Some(c), 0x01) => self.channels[c] = Channel::default(),
            (None, 0x01) => {}
            // DMAGO
            (None, op) if op & !0x02 == 0xa0 => {
                let c = &mut self.channels[(insn[1] & 7) as usize];
                c.pc = u32::from_le_bytes([insn[2], insn[3], insn[4], insn[5]]);
                c.state = State::Executing;
                c.fault = 0;
                c.fifo.clear();
            }
            // DMASEV
            (None, 0x34) => self.event(insn[1] >> 3),
            (None, 0x18) => {} // DMANOP
            _ => self.manager_fault |= FT_UNDEF_INSTR,
        }
    }

    fn event(&mut self, ev: u8) {
        let ev = ev as usize % EVENTS;
        self.es |= 1 << ev;
    }

    /// Run every thread that can make progress, until each ends or waits.
    pub fn run(&mut self, bus: &mut dyn Pl330Bus) {
        for c in 0..CHANNELS {
            if let State::Wfp(p) = self.channels[c].state {
                if bus.request(p) {
                    self.channels[c].state = State::Executing;
                } else {
                    continue;
                }
            }
            if self.channels[c].state == State::Executing {
                self.run_thread(c, bus);
            }
        }
    }

    fn run_thread(&mut self, c: usize, bus: &mut dyn Pl330Bus) {
        for _ in 0..STEP_BUDGET {
            if self.channels[c].state != State::Executing {
                return;
            }
            let pc = self.channels[c].pc;
            let mut insn = [0u8; 6];
            if !bus.read(pc, &mut insn) {
                return self.fault(c, FT_DATA_READ);
            }
            if !self.step(c, &insn, bus) {
                return;
            }
        }
    }

    fn fault(&mut self, c: usize, kind: u32) {
        let ch = &mut self.channels[c];
        if self.log {
            eprintln!("[pl330] channel {c} fault {kind:#x} at {:#x}", ch.pc);
        }
        ch.state = State::Fault;
        ch.fault |= kind;
    }

    /// Beat size and count of a load (`src`) or a store from CCR.
    fn burst(ccr: u32, src: bool, single: bool) -> (usize, usize, bool) {
        let (size, len, inc) = if src { ((ccr >> 1) & 7, (ccr >> 4) & 0xf, ccr & 1 != 0) } else { ((ccr >> 15) & 7, (ccr >> 18) & 0xf, ccr >> 14 & 1 != 0) };
        (1 << size, if single { 1 } else { len as usize + 1 }, inc)
    }

    /// One instruction; false when the thread stops or waits.
    fn step(&mut self, c: usize, insn: &[u8; 6], bus: &mut dyn Pl330Bus) -> bool {
        let op = insn[0];
        let ch = &mut self.channels[c];
        // the S/B condition of LD, ST, LPEND: x (bit 0) set means "only for this request type"
        let cond_ok = |op: u8, burst: bool| op & 1 == 0 || (op >> 1 & 1 != 0) == burst;
        let mut size = 1;
        match op {
            0x00 => {
                // DMAEND
                ch.state = State::Stopped;
                return false;
            }
            0x01 => {
                ch.state = State::Stopped;
                return false;
            }
            0x12 | 0x13 | 0x18 => {} // DMARMB, DMAWMB, DMANOP
            0x04..=0x0b => {
                // DMALD / DMAST [S|B]
                if cond_ok(op, ch.burst) {
                    // the S/B suffix only conditions the instruction; CCR sets the burst
                    let load = op < 0x08;
                    if !self.transfer(c, load, false, bus) {
                        return false;
                    }
                }
            }
            0x0c => {
                // DMASTZ: zeros for a whole store burst
                let ch = &mut self.channels[c];
                let (beat, beats, inc) = Self::burst(ch.ccr, false, false);
                let zeros = vec![0u8; beat * beats];
                if !bus.write(ch.dar, &zeros) {
                    self.fault(c, FT_DATA_WRITE);
                    return false;
                }
                if inc {
                    self.channels[c].dar = self.channels[c].dar.wrapping_add((beat * beats) as u32);
                }
            }
            0x20 | 0x22 => {
                // DMALP: the counter holds iterations - 1
                ch.lc[(op >> 1 & 1) as usize] = insn[1] as u32;
                size = 2;
            }
            0x25 | 0x27 | 0x29 | 0x2b => {
                // DMALDP / DMASTP S|B, peripheral in byte 1
                let burst = op & 2 != 0;
                if burst == ch.burst {
                    let load = op < 0x28;
                    if !self.transfer(c, load, !burst, bus) {
                        return false;
                    }
                }
                size = 2;
            }
            0x28..=0x3f if op & 0xe8 == 0x28 => {
                // DMALPEND[S|B], DMALPFE: bit 4 = counted loop, bit 2 = loop counter
                size = 2;
                let back = insn[1] as u32;
                let counted = op & 0x10 != 0;
                if cond_ok(op, ch.burst) {
                    let lc = &mut ch.lc[(op >> 2 & 1) as usize];
                    if !counted || *lc != 0 {
                        if counted {
                            *lc -= 1;
                        }
                        ch.pc = ch.pc.wrapping_sub(back);
                        return true;
                    }
                }
            }
            0x30..=0x32 => {
                // DMAWFP S|B|P
                let peri = insn[1] >> 3;
                ch.burst = op != 0x30;
                ch.pc = ch.pc.wrapping_add(2);
                if bus.request(peri) {
                    return true;
                }
                ch.state = State::Wfp(peri);
                return false;
            }
            0x34 => {
                // DMASEV
                ch.pc = ch.pc.wrapping_add(2);
                self.event(insn[1] >> 3);
                return true;
            }
            0x35 | 0x36 => size = 2, // DMAFLUSHP, DMAWFE (events are not waited for)
            0x54 | 0x56 => {
                // DMAADDH SAR|DAR
                let v = u16::from_le_bytes([insn[1], insn[2]]) as u32;
                if op & 2 == 0 { ch.sar = ch.sar.wrapping_add(v) } else { ch.dar = ch.dar.wrapping_add(v) }
                size = 3;
            }
            0xbc => {
                // DMAMOV SAR|CCR|DAR
                let v = u32::from_le_bytes([insn[2], insn[3], insn[4], insn[5]]);
                match insn[1] & 7 {
                    0 => ch.sar = v,
                    1 => ch.ccr = v,
                    2 => ch.dar = v,
                    _ => {
                        self.fault(c, FT_OPERAND_INVALID);
                        return false;
                    }
                }
                size = 6;
            }
            _ => {
                self.fault(c, FT_UNDEF_INSTR);
                return false;
            }
        }
        let ch = &mut self.channels[c];
        ch.pc = ch.pc.wrapping_add(size);
        true
    }

    /// A load into the MFIFO from SAR or a store from it to DAR.
    fn transfer(&mut self, c: usize, load: bool, single: bool, bus: &mut dyn Pl330Bus) -> bool {
        let ch = &mut self.channels[c];
        let (beat, beats, inc) = Self::burst(ch.ccr, load, single);
        let n = beat * beats;
        if load {
            let mut buf = vec![0u8; n];
            if !bus.read(ch.sar, &mut buf) {
                self.fault(c, FT_DATA_READ);
                return false;
            }
            ch.fifo.extend(buf);
            if inc {
                ch.sar = ch.sar.wrapping_add(n as u32);
            }
        } else {
            let data: Vec<u8> = (0..n).map(|_| ch.fifo.pop_front().unwrap_or(0)).collect();
            if !bus.write(ch.dar, &data) {
                self.fault(c, FT_DATA_WRITE);
                return false;
            }
            if inc {
                ch.dar = ch.dar.wrapping_add(n as u32);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Memory at 0 (program and source), a peripheral FIFO at 0x1000_0000.
    struct Bus {
        mem: Vec<u8>,
        fifo: Vec<u32>,
        req: bool,
    }

    impl Pl330Bus for Bus {
        fn read(&mut self, addr: u32, buf: &mut [u8]) -> bool {
            let a = addr as usize;
            match self.mem.get(a..a + buf.len()) {
                Some(s) => {
                    buf.copy_from_slice(s);
                    true
                }
                None => false,
            }
        }
        fn write(&mut self, addr: u32, data: &[u8]) -> bool {
            if addr == 0x1000_0000 {
                self.fifo.extend(data.chunks(4).map(|w| u32::from_le_bytes(w.try_into().unwrap())));
                return true;
            }
            let a = addr as usize;
            self.mem.get_mut(a..a + data.len()).map(|s| s.copy_from_slice(data)).is_some()
        }
        fn request(&mut self, _peri: u8) -> bool {
            self.req
        }
    }

    /// The microcode Linux builds for a memory-to-peripheral transfer of 4 bursts of
    /// 8 words with the periph-burst quirk, then SEV 3 and END.
    #[test]
    fn mem_to_peripheral_bursts_wait_for_requests() {
        let mut prog = Vec::new();
        let ccr: u32 = 1 | 2 << 1 | 7 << 4 | 2 << 15 | 7 << 18; // src inc, 4-byte beats, 8 beats
        prog.extend_from_slice(&[0xbc, 1]);
        prog.extend_from_slice(&ccr.to_le_bytes());
        prog.extend_from_slice(&[0xbc, 0]);
        prog.extend_from_slice(&0x100u32.to_le_bytes()); // SAR
        prog.extend_from_slice(&[0xbc, 2]);
        prog.extend_from_slice(&0x1000_0000u32.to_le_bytes()); // DAR
        prog.extend_from_slice(&[0x22, 3]); // DMALP lc1, 4 iterations
        let body = prog.len();
        prog.extend_from_slice(&[0x32, 0 << 3, 0x07, 0x2b, 0 << 3]); // WFPB 0, LDB, STPB 0
        let back = (prog.len() - body) as u8;
        prog.extend_from_slice(&[0x38 | 0x10 | 0x04, back]); // LPEND lc1
        prog.extend_from_slice(&[0x34, 3 << 3, 0x00]); // SEV 3, END
        let mut mem = vec![0u8; 0x1000];
        mem[..prog.len()].copy_from_slice(&prog);
        for (i, w) in mem[0x100..0x180].chunks_mut(4).enumerate() {
            w.copy_from_slice(&(i as u32).to_le_bytes());
        }
        let mut bus = Bus { mem, fifo: Vec::new(), req: false };
        let mut d = Pl330::new();
        assert_eq!(d.read(PERIPH_ID), 0x30);
        assert_eq!(d.read(0x3ff0), 0x0d, "the IDs at the end of a 16 KiB region");
        d.write(INTEN, 1 << 3, &mut bus);
        // DMAGO channel 0 at 0 through the debug interface
        d.write(DBGINST0, (0xa0 << 16) as u32, &mut bus);
        d.write(DBGINST1, 0, &mut bus);
        d.write(DBGCMD, 0, &mut bus);
        assert!(d.waiting_on(0));
        assert_eq!(d.read(CS) & 0xf, ST_WFP);
        // with the request held, the whole transfer runs
        bus.req = true;
        d.run(&mut bus);
        assert_eq!(bus.fifo.len(), 32);
        assert_eq!(bus.fifo[..3], [0, 1, 2]);
        assert_eq!(bus.fifo[31], 31);
        assert!(d.irq(), "SEV 3 with its interrupt enabled");
        assert_eq!(d.read(CS) & 0xf, ST_STOP);
        assert_eq!(d.read(SA), 0x180, "SAR advanced over the data");
        d.write(INTCLR, 1 << 3, &mut bus);
        assert!(!d.irq());
    }
}
