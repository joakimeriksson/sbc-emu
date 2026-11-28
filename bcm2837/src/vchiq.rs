//! VCHIQ, the message queue between Linux and the VideoCore firmware (`brcm,bcm2835-vchiq`),
//! from the VideoCore's side: the emulator plays the firmware's part of the protocol that the
//! Raspberry Pi kernel's `vchiq_core.c` / `vchiq_arm.c` implement on the ARM.
//!
//! Linux allocates the shared memory (slot zero, 4 KiB message slots for each side and the
//! bulk "fragments" area), fills in the layout and hands its bus address to the firmware with
//! the mailbox property `VCHIQ_INIT` (`Vchiq::init`). The VideoCore is the master: it owns the
//! `master` half of slot zero and the first half of the slots, Linux (the slave) the other.
//! Each side writes messages into its own slots (an 8-byte header `msgid`, `size`, then the
//! payload, padded to 8 bytes), advances its `tx_pos` and signals the other side's `trigger`
//! event; the receiver hands a slot back through the sender's `slot_queue` once it has read it
//! (`recycle`). Events are `armed`/`fired` pairs in slot zero; the doorbells carry them:
//! Linux rings the VideoCore's (BELL2, a write) and the VideoCore rings Linux's (BELL0, read to
//! clear, ARM interrupt 2).
//!
//! On top: services found by four-character code (`OPEN`/`OPENACK`/`CLOSE`), `DATA` messages,
//! and bulk transfers that the master performs: Linux sends `BULK_TX`/`BULK_RX` with the bus
//! address of a page list, the VideoCore copies the data and answers `BULK_TX_DONE` /
//! `BULK_RX_DONE` (the unaligned head and tail of a receive go through the fragments area).
//! The services modelled are MMAL (`mmal.rs`: the codec, ISP and image components), the
//! shared-memory service vc-sm-cma uses to hand buffers over (`SMEM`, there too) and the audio
//! service (`AUDS`, `vcaudio.rs`). Anything else is refused with `CLOSE`, as firmware without
//! the service would.
use crate::mmal::Mmal;
use crate::vcaudio::Audio;
use std::collections::VecDeque;

/// `VCHIQ_MAKE_FOURCC`: first character in the high byte.
pub const fn fourcc(s: &[u8; 4]) -> u32 { (s[0] as u32) << 24 | (s[1] as u32) << 16 | (s[2] as u32) << 8 | s[3] as u32 }

pub const MAGIC: u32 = fourcc(b"VCHI");
pub const SLOT: u32 = 4096;
const MAX_SLOTS_PER_SIDE: u32 = 64;
/// The doorbell registers (offsets in the block at 0x7e00_b840): BELL0 is the ARM's (the
/// VideoCore rang it; reading clears), BELL2 the VideoCore's (Linux writes it).
pub const BELL0: u64 = 0x00;
pub const BELL2: u64 = 0x08;
/// BELL0's "doorbell rung" bit (`ARM_DS_ACTIVE`)
const ARM_DS_ACTIVE: u32 = 1 << 2;

pub const MSG_PADDING: u32 = 0;
pub const MSG_CONNECT: u32 = 1;
pub const MSG_OPEN: u32 = 2;
pub const MSG_OPENACK: u32 = 3;
pub const MSG_CLOSE: u32 = 4;
pub const MSG_DATA: u32 = 5;
pub const MSG_BULK_RX: u32 = 6;
pub const MSG_BULK_TX: u32 = 7;
pub const MSG_BULK_RX_DONE: u32 = 8;
pub const MSG_BULK_TX_DONE: u32 = 9;

pub const fn make_msg(t: u32, src: u32, dst: u32) -> u32 { t << 24 | src << 12 | dst }
pub const fn msg_type(id: u32) -> u32 { id >> 24 }
pub const fn msg_src(id: u32) -> u32 { id >> 12 & 0xfff }
pub const fn msg_dst(id: u32) -> u32 { id & 0xfff }
/// A message's footprint in a slot: header plus payload, rounded up to the header size.
pub const fn stride(size: u32) -> u32 { (size + 8 + 7) & !7 }

/// Offsets in `struct vchiq_shared_state` (one per side in slot zero).
pub mod ss {
    pub const INITIALISED: u64 = 0;
    pub const SLOT_FIRST: u64 = 4;
    pub const SLOT_LAST: u64 = 8;
    pub const SLOT_SYNC: u64 = 12;
    pub const TRIGGER: u64 = 16;
    pub const TX_POS: u64 = 28;
    pub const RECYCLE: u64 = 32;
    pub const SLOT_QUEUE_RECYCLE: u64 = 44;
    pub const SYNC_TRIGGER: u64 = 48;
    pub const SYNC_RELEASE: u64 = 60;
    pub const SLOT_QUEUE: u64 = 72;
    /// a `struct remote_event`: armed, fired
    pub const ARMED: u64 = 0;
    pub const FIRED: u64 = 4;
}

/// RAM through the VideoCore's eyes: bus addresses in any of its aliases (Linux uses
/// 0xc000_0000) map to the same physical RAM.
pub fn phys(bus: u32) -> u64 { (bus & 0x3fff_ffff) as u64 }

pub fn r32(ram: &[u8], a: u64) -> u32 {
    let a = a as usize;
    ram.get(a..a + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
}
pub fn w32(ram: &mut [u8], a: u64, v: u32) {
    let a = a as usize;
    if let Some(b) = ram.get_mut(a..a + 4) { b.copy_from_slice(&v.to_le_bytes()); }
}
pub fn read_bytes(ram: &[u8], a: u64, n: usize) -> Vec<u8> {
    let a = (a as usize).min(ram.len());
    ram[a..(a + n).min(ram.len())].to_vec()
}
pub fn write_bytes(ram: &mut [u8], a: u64, d: &[u8]) {
    let a = (a as usize).min(ram.len());
    let n = d.len().min(ram.len() - a);
    ram[a..a + n].copy_from_slice(&d[..n]);
}

/// What a service hands back to Linux: a `DATA` message, or data for Linux's next bulk
/// receive on that service.
#[derive(Debug, PartialEq)]
pub enum Out {
    Data(Vec<u8>),
    Bulk(Vec<u8>),
}

/// Messages services produce while handling one, by the VCHIQ port (ours) of the connection.
#[derive(Default)]
pub struct Outbox(pub Vec<(u32, Out)>);

impl Outbox {
    pub fn data(&mut self, port: u32, d: Vec<u8>) { self.0.push((port, Out::Data(d))); }
    pub fn bulk(&mut self, port: u32, d: Vec<u8>) { self.0.push((port, Out::Bulk(d))); }
}

/// One open connection to a service.
struct Service {
    fourcc: u32,
    /// our port and Linux's
    local: u32,
    remote: u32,
    /// data waiting for Linux's bulk receives, and Linux's receives waiting for data
    /// (page list bus address, size)
    bulk_out: VecDeque<Vec<u8>>,
    rx_wait: VecDeque<(u32, u32)>,
}

pub const MMAL: u32 = fourcc(b"mmal");
pub const SMEM: u32 = fourcc(b"SMEM");
pub const AUDS: u32 = fourcc(b"AUDS");

/// The services the firmware offers: four-character code and the version it reports.
const OFFERED: &[(u32, u16)] = &[(MMAL, 15), (SMEM, 2), (AUDS, 2)];

pub struct Vchiq {
    /// physical address of slot zero; None until Linux's VCHIQ_INIT
    base: Option<u64>,
    /// `sizeof(struct vchiq_shared_state)` as Linux built it (from `slot_zero_size`)
    shared: u64,
    max_slots: u32,
    /// the bulk fragments area (bus address) and the size of one fragment pair
    frag_base: u32,
    pub cache_line: u32,
    /// BELL0 rung, not yet read
    bell0: bool,
    tx_pos: u32,
    rx_pos: u32,
    /// messages waiting for one of our slots to come back
    pending: VecDeque<(u32, Vec<u8>)>,
    services: Vec<Service>,
    next_port: u32,
    pub mmal: Mmal,
    pub audio: Audio,
    /// `SBC_VCHIQ_TRACE`: log every message
    pub trace: bool,
    /// messages in each direction (diagnostics)
    pub rx_msgs: u64,
    pub tx_msgs: u64,
}

impl Default for Vchiq { fn default() -> Self { Self::new() } }

impl Vchiq {
    pub fn new() -> Self {
        Vchiq {
            base: None, shared: 0, max_slots: 0, frag_base: 0, cache_line: 64, bell0: false, tx_pos: 0, rx_pos: 0, pending: VecDeque::new(),
            services: Vec::new(), next_port: 1, mmal: Mmal::new(), audio: Audio::new(),
            trace: std::env::var_os("SBC_VCHIQ_TRACE").is_some(), rx_msgs: 0, tx_msgs: 0,
        }
    }

    pub fn connected(&self) -> bool { self.base.is_some() }
    pub fn irq(&self) -> bool { self.bell0 }

    fn master(&self) -> u64 { self.base.unwrap_or(0) + 32 }
    fn slave(&self) -> u64 { self.master() + self.shared }
    fn slot_addr(&self, index: u32) -> u64 { self.base.unwrap_or(0) + index as u64 * SLOT as u64 }

    /// The mailbox's `VCHIQ_INIT` with the bus address of Linux's slot zero: check it, set up
    /// the master's half and mark it initialised (Linux then sends `CONNECT`). Returns false
    /// (the property's error) if the memory does not hold a slot zero this model understands.
    pub fn init(&mut self, ram: &mut [u8], bus: u32) -> bool {
        let base = phys(bus);
        let size = r32(ram, base + 8);
        let slot_size = r32(ram, base + 12);
        let max_slots = r32(ram, base + 16);
        let per_side = r32(ram, base + 20);
        if r32(ram, base) != MAGIC || slot_size != SLOT || per_side != MAX_SLOTS_PER_SIDE || max_slots == 0 || size < 32 + 4 * max_slots {
            eprintln!("[vchiq] VCHIQ_INIT at {bus:#x}: not a slot zero (magic {:#x}, slot size {slot_size})", r32(ram, base));
            return false;
        }
        let shared = (size - 32 - 4 * max_slots) as u64 / 2;
        // the shared state must hold at least the fields up to the slot queue
        if shared < ss::SLOT_QUEUE + 4 * MAX_SLOTS_PER_SIDE as u64 { return false; }
        *self = Vchiq { trace: self.trace, ..Vchiq::new() };
        self.base = Some(base);
        self.shared = shared;
        self.max_slots = max_slots;
        self.frag_base = r32(ram, base + 24);
        let m = self.master();
        let (first, last, sync) = (r32(ram, m + ss::SLOT_FIRST), r32(ram, m + ss::SLOT_LAST), r32(ram, m + ss::SLOT_SYNC));
        let n = last.saturating_sub(first) + 1;
        for k in 0..n.min(MAX_SLOTS_PER_SIDE) { w32(ram, m + ss::SLOT_QUEUE + 4 * k as u64, first + k); }
        w32(ram, m + ss::TX_POS, 0);
        w32(ram, m + ss::SLOT_QUEUE_RECYCLE, n.min(MAX_SLOTS_PER_SIDE));
        // our events stay armed: every signal from Linux rings our doorbell
        for ev in [ss::TRIGGER, ss::RECYCLE, ss::SYNC_TRIGGER, ss::SYNC_RELEASE] {
            w32(ram, m + ev + ss::ARMED, 1);
            w32(ram, m + ev + ss::FIRED, 0);
        }
        // our synchronous slot is empty and released
        w32(ram, self.slot_addr(sync), MSG_PADDING);
        w32(ram, m + ss::SYNC_RELEASE + ss::FIRED, 1);
        w32(ram, m + ss::INITIALISED, 1);
        if self.trace { eprintln!("[vchiq] slot zero at {bus:#x}: {} slots, master {first}..{last}, shared state {shared} bytes", max_slots); }
        true
    }

    /// A read of the doorbell block.
    pub fn read(&mut self, off: u64) -> u32 {
        match off {
            BELL0 => { let v = if self.bell0 { ARM_DS_ACTIVE } else { 0 }; self.bell0 = false; v }
            _ => 0,
        }
    }

    /// A write to the doorbell block: Linux rang ours (BELL2).
    pub fn write(&mut self, off: u64, _v: u32, ram: &mut [u8], now: u64) {
        if off == BELL2 { self.service(ram, now); }
    }

    /// Signal one of Linux's events and ring its doorbell.
    fn signal(&mut self, ram: &mut [u8], event: u64) {
        let a = self.slave() + event;
        w32(ram, a + ss::FIRED, 1);
        self.bell0 = true;
    }

    /// Handle what Linux signalled: its messages, slots it gave back, a synchronous message.
    pub fn service(&mut self, ram: &mut [u8], now: u64) {
        if self.base.is_none() { return; }
        let m = self.master();
        for ev in [ss::TRIGGER, ss::RECYCLE, ss::SYNC_TRIGGER, ss::SYNC_RELEASE] { w32(ram, m + ev + ss::FIRED, 0); }
        // a synchronous message (no kernel service of ours uses them): drop it and release the slot
        let s = self.slave();
        let sync = self.slot_addr(r32(ram, s + ss::SLOT_SYNC));
        if r32(ram, sync) != MSG_PADDING {
            if self.trace { eprintln!("[vchiq] synchronous message {:#x} ignored", r32(ram, sync)); }
            w32(ram, sync, MSG_PADDING);
            self.signal(ram, ss::SYNC_RELEASE);
        }
        let mut out = Outbox::default();
        self.receive(ram, now, &mut out);
        self.deliver(ram, out);
        self.flush(ram);
    }

    /// Periodic work: the audio service plays out its data, the media components finish what
    /// they could not when the buffers came.
    pub fn tick(&mut self, ram: &mut [u8], now: u64) {
        if self.base.is_none() { return; }
        let mut out = Outbox::default();
        self.audio.tick(now, &mut out);
        self.mmal.tick(ram, now, &mut out);
        if !out.0.is_empty() { self.deliver(ram, out); self.flush(ram); }
    }

    /// When `tick` wants to run next.
    pub fn next_event(&self) -> Option<u64> { if self.base.is_none() { None } else { self.audio.next_event() } }

    /// Read Linux's messages up to its `tx_pos`.
    fn receive(&mut self, ram: &mut [u8], now: u64, out: &mut Outbox) {
        let s = self.slave();
        // Linux's slots hold at most this many messages: a bound against a corrupt queue
        for _ in 0..MAX_SLOTS_PER_SIDE * SLOT / 8 {
            let tx = r32(ram, s + ss::TX_POS);
            if self.rx_pos == tx { break; }
            let slot = r32(ram, s + ss::SLOT_QUEUE + 4 * ((self.rx_pos / SLOT) % MAX_SLOTS_PER_SIDE) as u64);
            if slot >= self.max_slots { eprintln!("[vchiq] bad slot {slot} in Linux's queue"); self.rx_pos = tx; break; }
            let hdr = self.slot_addr(slot) + (self.rx_pos % SLOT) as u64;
            let (msgid, size) = (r32(ram, hdr), r32(ram, hdr + 4));
            if (self.rx_pos % SLOT) + stride(size) > SLOT {
                eprintln!("[vchiq] message {msgid:#x} of {size} bytes overruns its slot");
                self.rx_pos = tx;
                break;
            }
            let payload = read_bytes(ram, hdr + 8, size as usize);
            self.rx_pos = self.rx_pos.wrapping_add(stride(size));
            if self.rx_pos % SLOT == 0 {
                // done with that slot: back to Linux
                let q = r32(ram, s + ss::SLOT_QUEUE_RECYCLE);
                w32(ram, s + ss::SLOT_QUEUE + 4 * (q % MAX_SLOTS_PER_SIDE) as u64, slot);
                w32(ram, s + ss::SLOT_QUEUE_RECYCLE, q.wrapping_add(1));
                self.signal(ram, ss::RECYCLE);
            }
            self.rx_msgs += 1;
            self.dispatch(ram, now, msgid, &payload, out);
        }
    }

    fn dispatch(&mut self, ram: &mut [u8], now: u64, msgid: u32, p: &[u8], out: &mut Outbox) {
        let (src, dst) = (msg_src(msgid), msg_dst(msgid));
        let word = |k: usize| p.get(4 * k..4 * k + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
        if self.trace && msg_type(msgid) != MSG_PADDING { eprintln!("[vchiq] <- type {} {src}->{dst} {} bytes", msg_type(msgid), p.len()); }
        match msg_type(msgid) {
            MSG_CONNECT => self.queue(make_msg(MSG_CONNECT, 0, 0), Vec::new()),
            MSG_OPEN => {
                let (fcc, version_min) = (word(0), (word(2) >> 16) as u16);
                match OFFERED.iter().find(|(f, _)| *f == fcc) {
                    Some(&(_, ours)) if ours >= version_min => {
                        let local = self.next_port;
                        self.next_port = if self.next_port >= 0xfff { 1 } else { self.next_port + 1 };
                        self.services.push(Service { fourcc: fcc, local, remote: src, bulk_out: VecDeque::new(), rx_wait: VecDeque::new() });
                        self.queue(make_msg(MSG_OPENACK, local, src), ours.to_le_bytes().to_vec());
                        if self.trace { eprintln!("[vchiq] service {:?} open on port {local}", fourcc_str(fcc)); }
                    }
                    _ => {
                        if self.trace { eprintln!("[vchiq] service {:?} refused", fourcc_str(fcc)); }
                        self.queue(make_msg(MSG_CLOSE, 0, src), Vec::new());
                    }
                }
            }
            MSG_CLOSE => {
                if let Some(k) = self.services.iter().position(|s| s.local == dst) {
                    let s = self.services.remove(k);
                    match s.fourcc { MMAL => self.mmal.closed(s.local), AUDS => self.audio.closed(s.local), _ => {} }
                    self.queue(make_msg(MSG_CLOSE, s.local, s.remote), Vec::new());
                }
            }
            MSG_DATA => {
                let Some(s) = self.services.iter().find(|s| s.local == dst && s.remote == src) else { return };
                let (fcc, port) = (s.fourcc, s.local);
                match fcc {
                    MMAL => self.mmal.message(port, p, ram, now, out),
                    SMEM => self.mmal.smem.message(port, p, out),
                    AUDS => self.audio.message(port, p, now, out),
                    _ => {}
                }
            }
            MSG_BULK_TX => {
                // Linux sends: we read its pages
                let (list, size) = (word(0), word(1));
                let Some(s) = self.services.iter().find(|s| s.local == dst) else { return };
                let (fcc, local, remote) = (s.fourcc, s.local, s.remote);
                let data = self.pagelist_read(ram, list, size);
                if fcc == AUDS { self.audio.bulk(local, &data, now, out); }
                self.queue(make_msg(MSG_BULK_TX_DONE, local, remote), (data.len() as u32).to_le_bytes().to_vec());
            }
            MSG_BULK_RX => {
                let (list, size) = (word(0), word(1));
                let Some(k) = self.services.iter().position(|s| s.local == dst) else { return };
                self.services[k].rx_wait.push_back((list, size));
                self.bulk_pump(ram, k);
            }
            // PAUSE/RESUME (suspend) and the keep-alive use counts need no answer from us
            _ => {}
        }
    }

    /// Send what the services produced.
    fn deliver(&mut self, ram: &mut [u8], out: Outbox) {
        for (port, o) in out.0 {
            let Some(k) = self.services.iter().position(|s| s.local == port) else { continue };
            match o {
                Out::Data(d) => { let s = &self.services[k]; let id = make_msg(MSG_DATA, s.local, s.remote); self.queue(id, d); }
                Out::Bulk(d) => { self.services[k].bulk_out.push_back(d); self.bulk_pump(ram, k); }
            }
        }
    }

    /// Match Linux's bulk receives with data for them.
    fn bulk_pump(&mut self, ram: &mut [u8], k: usize) {
        while !self.services[k].rx_wait.is_empty() && !self.services[k].bulk_out.is_empty() {
            let (list, size) = self.services[k].rx_wait.pop_front().unwrap();
            let data = self.services[k].bulk_out.pop_front().unwrap();
            let n = self.pagelist_write(ram, list, &data[..data.len().min(size as usize)]);
            let s = &self.services[k];
            let id = make_msg(MSG_BULK_RX_DONE, s.local, s.remote);
            self.queue(id, (n as u32).to_le_bytes().to_vec());
        }
    }

    /// The (physical address, length) runs of a page list (`struct pagelist`): length, type,
    /// offset into the first page, then page addresses whose low 12 bits count the
    /// consecutive pages that follow.
    fn pagelist_runs(&self, ram: &[u8], list: u32) -> (Vec<(u64, usize)>, u32, u32) {
        let l = phys(list);
        let length = r32(ram, l);
        let tw = r32(ram, l + 4);
        let (kind, offset) = (tw & 0xffff, tw >> 16);
        let mut runs = Vec::new();
        let mut left = length as usize;
        let mut k = 0;
        while left > 0 && k < 1 << 16 {
            let a = r32(ram, l + 8 + 4 * k as u64);
            let pages = (a & 0xfff) as usize + 1;
            let start = phys(a & !0xfff) + if k == 0 { offset as u64 } else { 0 };
            let n = (pages * SLOT as usize - if k == 0 { offset as usize } else { 0 }).min(left);
            runs.push((start, n));
            left -= n;
            k += 1;
        }
        (runs, kind, offset)
    }

    /// Gather a bulk transmit's data.
    fn pagelist_read(&self, ram: &[u8], list: u32, size: u32) -> Vec<u8> {
        let (runs, _, _) = self.pagelist_runs(ram, list);
        let mut v = Vec::with_capacity(size as usize);
        for (a, n) in runs { v.extend_from_slice(&read_bytes(ram, a, n)); }
        v.truncate(size as usize);
        v
    }

    /// Scatter a bulk receive's data; the partial cache lines at either end also go to the
    /// page list's fragment pair, which Linux copies over its pages afterwards.
    fn pagelist_write(&self, ram: &mut [u8], list: u32, data: &[u8]) -> usize {
        let (runs, kind, offset) = self.pagelist_runs(ram, list);
        let mut p = 0;
        for (a, n) in runs {
            let n = n.min(data.len() - p);
            write_bytes(ram, a, &data[p..p + n]);
            p += n;
            if p == data.len() { break; }
        }
        if kind >= 2 && self.frag_base != 0 {
            let cl = self.cache_line as usize;
            let frag = phys(self.frag_base) + ((kind - 2) as usize * 2 * cl) as u64;
            let head = ((cl - offset as usize % cl) % cl).min(p);
            write_bytes(ram, frag, &data[..head]);
            let tail = (offset as usize + p) % cl;
            if head < p && tail != 0 { write_bytes(ram, frag + cl as u64, &data[p - tail..p]); }
        }
        p
    }

    /// Queue a message to Linux (sent at once if one of our slots has room).
    fn queue(&mut self, msgid: u32, payload: Vec<u8>) {
        if self.trace && msg_type(msgid) != MSG_PADDING { eprintln!("[vchiq] -> type {} {}->{} {} bytes", msg_type(msgid), msg_src(msgid), msg_dst(msgid), payload.len()); }
        self.pending.push_back((msgid, payload));
    }

    /// Write queued messages into our slots while there is room, then tell Linux.
    fn flush(&mut self, ram: &mut [u8]) {
        let before = self.tx_pos;
        while let Some((id, p)) = self.pending.pop_front() {
            if !self.write_msg(ram, id, &p) { self.pending.push_front((id, p)); break; }
            self.tx_msgs += 1;
        }
        if self.tx_pos != before {
            w32(ram, self.master() + ss::TX_POS, self.tx_pos);
            self.signal(ram, ss::TRIGGER);
        }
    }

    fn write_msg(&mut self, ram: &mut [u8], msgid: u32, payload: &[u8]) -> bool {
        let m = self.master();
        let st = stride(payload.len() as u32);
        if st > SLOT { eprintln!("[vchiq] message of {} bytes dropped (larger than a slot)", payload.len()); return true; }
        let mut pos = self.tx_pos;
        let space = SLOT - pos % SLOT;
        if st > space {
            // pad out the rest of this slot
            let slot = r32(ram, m + ss::SLOT_QUEUE + 4 * ((pos / SLOT) % MAX_SLOTS_PER_SIDE) as u64);
            let a = self.slot_addr(slot) + (pos % SLOT) as u64;
            w32(ram, a, MSG_PADDING);
            w32(ram, a + 4, space - 8);
            pos = pos.wrapping_add(space);
            self.tx_pos = pos;
        }
        if pos % SLOT == 0 {
            // a new slot: only one Linux has handed back (or one never used)
            let avail = r32(ram, m + ss::SLOT_QUEUE_RECYCLE);
            if ((pos / SLOT).wrapping_sub(avail) as i32) >= 0 { return false; }
        }
        let slot = r32(ram, m + ss::SLOT_QUEUE + 4 * ((pos / SLOT) % MAX_SLOTS_PER_SIDE) as u64);
        let a = self.slot_addr(slot) + (pos % SLOT) as u64;
        w32(ram, a + 4, payload.len() as u32);
        write_bytes(ram, a + 8, payload);
        w32(ram, a, msgid);
        self.tx_pos = pos.wrapping_add(st);
        true
    }

    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        w.tag("vchiq-v1");
        w.opt_u64(self.base);
        w.u64(self.shared); w.u32(self.max_slots); w.u32(self.frag_base); w.u32(self.cache_line);
        w.bool(self.bell0); w.u32(self.tx_pos); w.u32(self.rx_pos); w.u32(self.next_port);
        w.u32(self.pending.len() as u32);
        for (id, p) in &self.pending { w.u32(*id); w.bytes(p); }
        w.u32(self.services.len() as u32);
        for s in &self.services {
            w.u32(s.fourcc); w.u32(s.local); w.u32(s.remote);
            w.u32(s.bulk_out.len() as u32);
            for d in &s.bulk_out { w.bytes(d); }
            w.u32(s.rx_wait.len() as u32);
            for (a, n) in &s.rx_wait { w.u32(*a); w.u32(*n); }
        }
        self.mmal.save_state(w);
        self.audio.save_state(w);
    }

    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("vchiq-v1")?;
        self.base = r.opt_u64()?;
        self.shared = r.u64()?; self.max_slots = r.u32()?; self.frag_base = r.u32()?; self.cache_line = r.u32()?;
        self.bell0 = r.bool()?; self.tx_pos = r.u32()?; self.rx_pos = r.u32()?; self.next_port = r.u32()?;
        self.pending.clear();
        for _ in 0..r.u32()? { let id = r.u32()?; self.pending.push_back((id, r.bytes()?.to_vec())); }
        self.services.clear();
        for _ in 0..r.u32()? {
            let (fourcc, local, remote) = (r.u32()?, r.u32()?, r.u32()?);
            let mut s = Service { fourcc, local, remote, bulk_out: VecDeque::new(), rx_wait: VecDeque::new() };
            for _ in 0..r.u32()? { s.bulk_out.push_back(r.bytes()?.to_vec()); }
            for _ in 0..r.u32()? { let a = r.u32()?; s.rx_wait.push_back((a, r.u32()?)); }
            self.services.push(s);
        }
        self.mmal.load_state(r)?;
        self.audio.load_state(r)
    }
}

pub fn fourcc_str(f: u32) -> String { f.to_be_bytes().iter().map(|&b| if b.is_ascii_graphic() { b as char } else { '?' }).collect() }

#[cfg(test)]
pub(crate) mod tests {
    //! A model of Linux's side (`vchiq_core.c` as the slave) drives the VideoCore model.
    use super::*;

    pub(crate) const SHARED: u64 = 372; // with VCHIQ_ENABLE_DEBUG: 11 debug words
    pub(crate) const BASE_BUS: u32 = 0xc010_0000;
    pub(crate) const MAX_SLOTS: u32 = 128;

    pub(crate) struct Arm {
        pub ram: Vec<u8>,
        pub base: u64,
        tx_pos: u32,
        rx_pos: u32,
        /// slots handed back to us so far (our slot_queue_available)
        pub vc: Vchiq,
        pub received: Vec<(u32, Vec<u8>)>,
    }

    impl Arm {
        pub fn master(&self) -> u64 { self.base + 32 }
        pub fn slave(&self) -> u64 { self.base + 32 + SHARED }

        /// `vchiq_init_slots` + `vchiq_init_state` for 65 slots (slot zero and 32 per side),
        /// then the mailbox call.
        pub fn new() -> Arm {
            let mut ram = vec![0u8; 8 << 20];
            let base = phys(BASE_BUS);
            let slot_zero_size = 32 + 2 * SHARED as u32 + 4 * MAX_SLOTS;
            w32(&mut ram, base, MAGIC);
            w32(&mut ram, base + 4, 8 | 3 << 16);
            w32(&mut ram, base + 8, slot_zero_size);
            w32(&mut ram, base + 12, SLOT);
            w32(&mut ram, base + 16, MAX_SLOTS);
            w32(&mut ram, base + 20, 64);
            w32(&mut ram, base + 24, BASE_BUS + 65 * SLOT); // fragments
            let (m, s) = (base + 32, base + 32 + SHARED);
            // slots 1..65: master sync 1, 2..32; slave sync 33, 34..64
            for (st, sync, first, last) in [(m, 1, 2, 32), (s, 33, 34, 64)] {
                w32(&mut ram, st + ss::SLOT_SYNC, sync);
                w32(&mut ram, st + ss::SLOT_FIRST, first);
                w32(&mut ram, st + ss::SLOT_LAST, last);
            }
            for k in 0..31 { w32(&mut ram, s + ss::SLOT_QUEUE + 4 * k, 34 + k as u32); }
            w32(&mut ram, s + ss::SLOT_QUEUE_RECYCLE, 31);
            w32(&mut ram, base + 33 * SLOT as u64, MSG_PADDING);
            w32(&mut ram, s + ss::INITIALISED, 1);
            let mut vc = Vchiq::new();
            assert!(vc.init(&mut ram, BASE_BUS));
            assert_eq!(r32(&ram, m + ss::INITIALISED), 1);
            Arm { ram, base, tx_pos: 0, rx_pos: 0, vc, received: Vec::new() }
        }

        /// `queue_message`: into our slots, then ring BELL2 (the VideoCore's trigger is armed).
        pub fn send(&mut self, msgid: u32, payload: &[u8]) {
            let s = self.slave();
            let st = stride(payload.len() as u32);
            let space = SLOT - self.tx_pos % SLOT;
            if st > space {
                let slot = r32(&self.ram, s + ss::SLOT_QUEUE + 4 * ((self.tx_pos / SLOT) % 64) as u64);
                let a = self.base + slot as u64 * 4096 + (self.tx_pos % SLOT) as u64;
                w32(&mut self.ram, a, MSG_PADDING);
                w32(&mut self.ram, a + 4, space - 8);
                self.tx_pos += space;
            }
            if self.tx_pos % SLOT == 0 {
                let avail = r32(&self.ram, s + ss::SLOT_QUEUE_RECYCLE);
                assert!(self.tx_pos / SLOT < avail, "Linux would stall here: the VideoCore kept its slots");
            }
            let slot = r32(&self.ram, s + ss::SLOT_QUEUE + 4 * ((self.tx_pos / SLOT) % 64) as u64);
            let a = self.base + slot as u64 * 4096 + (self.tx_pos % SLOT) as u64;
            w32(&mut self.ram, a, msgid);
            w32(&mut self.ram, a + 4, payload.len() as u32);
            write_bytes(&mut self.ram, a + 8, payload);
            self.tx_pos += st;
            w32(&mut self.ram, s + ss::TX_POS, self.tx_pos);
            assert_eq!(r32(&self.ram, self.master() + ss::TRIGGER), 1, "VideoCore trigger armed");
            let mt = self.master() + ss::TRIGGER + 4;
            w32(&mut self.ram, mt, 1);
            self.vc.write(BELL2, 0, &mut self.ram, 0);
            self.poll();
        }

        /// The doorbell interrupt: `parse_rx_slots`, releasing each slot of the VideoCore's
        /// as soon as it is read (as `vchiq_release_message` eventually does).
        pub fn poll(&mut self) {
            if self.vc.read(BELL0) & ARM_DS_ACTIVE == 0 { return; }
            let m = self.master();
            loop {
                let tx = r32(&self.ram, m + ss::TX_POS);
                if self.rx_pos == tx { break; }
                let slot = r32(&self.ram, m + ss::SLOT_QUEUE + 4 * ((self.rx_pos / SLOT) % 64) as u64);
                let a = self.base + slot as u64 * 4096 + (self.rx_pos % SLOT) as u64;
                let (id, size) = (r32(&self.ram, a), r32(&self.ram, a + 4));
                if msg_type(id) != MSG_PADDING { self.received.push((id, read_bytes(&self.ram, a + 8, size as usize))); }
                self.rx_pos += stride(size);
                if self.rx_pos % SLOT == 0 {
                    let q = r32(&self.ram, m + ss::SLOT_QUEUE_RECYCLE);
                    w32(&mut self.ram, m + ss::SLOT_QUEUE + 4 * (q % 64) as u64, slot);
                    w32(&mut self.ram, m + ss::SLOT_QUEUE_RECYCLE, q + 1);
                    self.vc.write(BELL2, 0, &mut self.ram, 0);
                }
            }
        }

        /// Connect and open `fcc`; returns (our port, the VideoCore's).
        pub fn open(&mut self, fcc: u32, port: u32, version: u16) -> Option<u32> {
            if r32(&self.ram, self.master() + ss::TX_POS) == 0 && self.received.is_empty() {
                self.send(make_msg(MSG_CONNECT, 0, 0), &[]);
                assert_eq!(msg_type(self.received.remove(0).0), MSG_CONNECT);
            }
            let mut p = fcc.to_le_bytes().to_vec();
            p.extend_from_slice(&0u32.to_le_bytes());
            p.extend_from_slice(&(version as u32 | (version as u32) << 16).to_le_bytes());
            self.send(make_msg(MSG_OPEN, port, 0), &p);
            let (id, payload) = self.received.remove(0);
            match msg_type(id) {
                MSG_OPENACK => { assert_eq!(msg_dst(id), port); assert_eq!(payload.len(), 2); Some(msg_src(id)) }
                MSG_CLOSE => None,
                t => panic!("unexpected reply {t}"),
            }
        }
    }

    #[test]
    fn connect_open_and_refuse() {
        let mut a = Arm::new();
        assert!(a.open(MMAL, 5, 15).is_some());
        assert!(a.open(fourcc(b"GCMD"), 6, 1).is_none(), "unknown services are refused");
        assert!(a.open(AUDS, 7, 2).is_some());
        let vc_port = a.open(SMEM, 8, 0).unwrap();
        // closing: the VideoCore answers with its own CLOSE
        a.send(make_msg(MSG_CLOSE, 8, vc_port), &[]);
        let (id, _) = a.received.remove(0);
        assert_eq!((msg_type(id), msg_src(id), msg_dst(id)), (MSG_CLOSE, vc_port, 8));
    }

    #[test]
    fn slots_wrap_and_are_recycled_both_ways() {
        let mut a = Arm::new();
        let port = a.open(SMEM, 3, 2).unwrap();
        // 3000 version messages from Linux cross its 31 slots several times; the VideoCore
        // must give every slot back or Linux stalls (asserted in `send`)
        let mut msg = vec![0u8; 12];
        msg[0] = 9; // VC_SM_MSG_TYPE_CLIENT_VERSION: no answer
        for k in 0..3000u32 {
            msg[4..8].copy_from_slice(&k.to_le_bytes());
            a.send(make_msg(MSG_DATA, 3, port), &msg);
        }
        // and imports, which are answered: 9000 16-byte replies wrap the VideoCore's 31 slots
        let mut imp = vec![0u8; 8 + 52];
        imp[0] = 8;
        for k in 0..9000u32 {
            imp[4..8].copy_from_slice(&k.to_le_bytes());
            imp[12..16].copy_from_slice(&0xc020_0000u32.to_le_bytes());
            imp[16..20].copy_from_slice(&4096u32.to_le_bytes());
            a.send(make_msg(MSG_DATA, 3, port), &imp);
            let (id, p) = a.received.pop().unwrap();
            assert_eq!(msg_type(id), MSG_DATA);
            assert_eq!(u32::from_le_bytes(p[0..4].try_into().unwrap()), k, "the reply carries the transaction id");
            let h = u32::from_le_bytes(p[4..8].try_into().unwrap());
            assert_ne!(h, 0);
            // free it again (no reply)
            let mut f = vec![0u8; 16];
            f[0] = 4;
            f[8..12].copy_from_slice(&h.to_le_bytes());
            a.send(make_msg(MSG_DATA, 3, port), &f);
        }
        assert!(a.vc.pending.is_empty());
        assert!(a.vc.tx_pos > 31 * SLOT);
    }

    #[test]
    fn bulk_receive_through_a_page_list_with_fragments() {
        let mut a = Arm::new();
        let port = a.open(MMAL, 4, 15).unwrap();
        // a page list for a receive of 5000 bytes starting 100 bytes into a page
        let list = 0xc030_0000u32;
        let (buf, off, len) = (0x0040_0000u32, 100u32, 5000u32);
        w32(&mut a.ram, phys(list), len);
        w32(&mut a.ram, phys(list) + 4, 2 | off << 16); // PAGELIST_READ_WITH_FRAGMENTS, fragment pair 0
        w32(&mut a.ram, phys(list) + 8, (0xc000_0000 | buf) | 1); // two pages from buf
        let data: Vec<u8> = (0..len).map(|k| (k * 7 % 251) as u8).collect();
        let k = a.vc.services.iter().position(|s| s.local == port).unwrap();
        a.vc.services[k].bulk_out.push_back(data.clone());
        a.send(make_msg(MSG_BULK_RX, 4, port), &[list.to_le_bytes(), len.to_le_bytes()].concat());
        let (id, p) = a.received.pop().unwrap();
        assert_eq!(msg_type(id), MSG_BULK_RX_DONE);
        assert_eq!(u32::from_le_bytes(p[..4].try_into().unwrap()), len);
        assert_eq!(read_bytes(&a.ram, (buf + off) as u64, len as usize), data);
        // the fragments: the head up to the next 64-byte line, the tail from the last line
        let frag = phys(BASE_BUS + 65 * SLOT) as usize;
        let head = (64 - off % 64) as usize;
        assert_eq!(&a.ram[frag..frag + head], &data[..head]);
        let tail = ((off + len) % 64) as usize;
        assert_eq!(&a.ram[frag + 64..frag + 64 + tail], &data[len as usize - tail..]);
    }

    #[test]
    fn snapshot_keeps_the_connection() {
        let mut a = Arm::new();
        let port = a.open(SMEM, 3, 2).unwrap();
        let mut imp = vec![0u8; 8 + 52];
        imp[0] = 8;
        imp[12..16].copy_from_slice(&0xc020_0000u32.to_le_bytes());
        a.send(make_msg(MSG_DATA, 3, port), &imp);
        let mut w = emu_core::snap::W::default();
        a.vc.save_state(&mut w);
        let mut vc = Vchiq::new();
        vc.load_state(&mut emu_core::snap::R::new(&w.0)).unwrap();
        assert_eq!(vc.mmal.smem.lookup(1), Some((0x20_0000, 0)));
        a.vc = vc;
        a.send(make_msg(MSG_DATA, 3, port), &imp);
        let (id, p) = a.received.pop().unwrap();
        assert_eq!((msg_type(id), u32::from_le_bytes(p[4..8].try_into().unwrap())), (MSG_DATA, 2), "the next handle, on the same connection");
    }

    #[test]
    fn bulk_transmit_is_read_and_acknowledged() {
        let mut a = Arm::new();
        let port = a.open(AUDS, 2, 2).unwrap();
        let list = 0xc030_0000u32;
        w32(&mut a.ram, phys(list), 6000);
        w32(&mut a.ram, phys(list) + 4, 0); // PAGELIST_WRITE
        w32(&mut a.ram, phys(list) + 8, 0xc050_0000 | 1);
        a.send(make_msg(MSG_BULK_TX, 2, port), &[list.to_le_bytes(), 6000u32.to_le_bytes()].concat());
        let (id, p) = a.received.pop().unwrap();
        assert_eq!((msg_type(id), u32::from_le_bytes(p[..4].try_into().unwrap())), (MSG_BULK_TX_DONE, 6000));
    }
}
