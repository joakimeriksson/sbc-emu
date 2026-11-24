//! An NVMe SSD for the Orange Pi RV's M.2 M-key slot (PCIe1), driven by Linux's `nvme`:
//! an NVMe 1.4 controller with one namespace of 512-byte blocks backed by a disk image.
//!
//! Registers: CAP (64 queue entries, 4 KiB pages, NVM command set), VS, CC/CSTS (enable and
//! shutdown complete at once), AQA/ASQ/ACQ and the doorbells (stride 4). Commands run when
//! their submission queue's tail doorbell is written; each completion is posted to the
//! queue's completion queue with its phase tag, and a completion queue with interrupts
//! enabled asks for an MSI. Data moves through PRP1/PRP2 and PRP lists.
//! Admin: Identify (controller, namespace, active namespace list, descriptors), Create/Delete
//! I/O SQ and CQ, Get/Set Features (number of queues and the rest stored), Get Log Page
//! (zeroes), Asynchronous Event Request (held), Abort. I/O: Read, Write, Flush, Write
//! Zeroes, Dataset Management (accepted).
use emu_core::disk::Disk;

const CC_EN: u32 = 1;
const PAGE: u64 = 4096;
const MAX_QUEUES: usize = 17;
/// largest transfer: 2^MDTS pages
const MDTS: u8 = 5;

#[derive(Clone, Copy, Default)]
struct Sq {
    base: u64,
    size: u32,
    head: u32,
    tail: u32,
    cq: usize,
    live: bool,
}

#[derive(Clone, Copy, Default)]
struct Cq {
    base: u64,
    size: u32,
    tail: u32,
    phase: bool,
    ien: bool,
    live: bool,
}

pub struct Nvme {
    pub disk: Option<Box<dyn Disk + Send>>,
    cc: u32,
    csts: u32,
    aqa: u32,
    asq: u64,
    acq: u64,
    intms: u32,
    sq: [Sq; MAX_QUEUES],
    cq: [Cq; MAX_QUEUES],
    features: std::collections::BTreeMap<u8, u32>,
    /// a completion with interrupts enabled was posted: the endpoint sends an MSI
    pub irq_request: bool,
    pub reads: u64,
    pub writes: u64,
}

fn rd(ram: &[u8], base: u64, a: u64, n: usize) -> Option<&[u8]> {
    let o = a.checked_sub(base)? as usize;
    ram.get(o..o.checked_add(n)?)
}

fn wr(ram: &mut [u8], base: u64, a: u64, data: &[u8]) -> bool {
    let Some(o) = a.checked_sub(base).map(|o| o as usize) else { return false };
    match ram.get_mut(o..o + data.len()) {
        Some(d) => {
            d.copy_from_slice(data);
            true
        }
        None => false,
    }
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

impl Nvme {
    pub fn new(disk: Option<Box<dyn Disk + Send>>) -> Self {
        Nvme {
            disk,
            cc: 0,
            csts: 0,
            aqa: 0,
            asq: 0,
            acq: 0,
            intms: 0,
            sq: [Sq::default(); MAX_QUEUES],
            cq: [Cq::default(); MAX_QUEUES],
            features: Default::default(),
            irq_request: false,
            reads: 0,
            writes: 0,
        }
    }

    fn reset(&mut self) {
        let disk = self.disk.take();
        let (reads, writes) = (self.reads, self.writes);
        *self = Nvme::new(disk);
        self.reads = reads;
        self.writes = writes;
    }

    fn cap(&self) -> u64 {
        // MQES 63, CQR, AMS 0, TO 10 s, DSTRD 0, NSSRS 0, CSS NVM, MPSMIN/MAX 4 KiB
        63 | (1 << 16) | (20 << 24) | (1 << 37)
    }

    pub fn read(&mut self, off: u64) -> u32 {
        match off {
            0x00 => self.cap() as u32,
            0x04 => (self.cap() >> 32) as u32,
            0x08 => 0x0001_0400, // 1.4
            0x0c | 0x10 => self.intms,
            0x14 => self.cc,
            0x1c => self.csts,
            0x24 => self.aqa,
            0x28 => self.asq as u32,
            0x2c => (self.asq >> 32) as u32,
            0x30 => self.acq as u32,
            0x34 => (self.acq >> 32) as u32,
            _ => 0,
        }
    }

    pub fn write(&mut self, off: u64, v: u32, ram: &mut [u8], base: u64) {
        match off {
            0x0c => self.intms |= v,
            0x10 => self.intms &= !v,
            0x14 => {
                let was = self.cc & CC_EN != 0;
                self.cc = v;
                if v & CC_EN != 0 && !was {
                    self.sq[0] = Sq { base: self.asq & !0xfff, size: (self.aqa & 0xfff) + 1, cq: 0, live: true, ..Sq::default() };
                    self.cq[0] = Cq { base: self.acq & !0xfff, size: ((self.aqa >> 16) & 0xfff) + 1, phase: true, ien: true, live: true, tail: 0 };
                    self.csts = 1; // RDY
                } else if v & CC_EN == 0 && was {
                    let (aqa, asq, acq) = (self.aqa, self.asq, self.acq);
                    self.reset();
                    self.aqa = aqa;
                    self.asq = asq;
                    self.acq = acq;
                }
                if (v >> 14) & 3 != 0 {
                    self.csts = (self.csts & !(3 << 2)) | (2 << 2); // shutdown complete
                }
            }
            0x24 => self.aqa = v,
            0x28 => self.asq = (self.asq & !0xffff_ffff) | v as u64,
            0x2c => self.asq = (self.asq & 0xffff_ffff) | (v as u64) << 32,
            0x30 => self.acq = (self.acq & !0xffff_ffff) | v as u64,
            0x34 => self.acq = (self.acq & 0xffff_ffff) | (v as u64) << 32,
            0x1000.. => {
                let q = ((off - 0x1000) / 8) as usize;
                let cq_doorbell = (off - 0x1000) % 8 == 4;
                if q >= MAX_QUEUES || self.csts & 1 == 0 {
                    return;
                }
                if cq_doorbell {
                    // the host consumed completions up to `v`; nothing to do
                } else if self.sq[q].live {
                    self.sq[q].tail = v % self.sq[q].size;
                    self.run(q, ram, base);
                }
            }
            _ => {}
        }
    }

    fn run(&mut self, q: usize, ram: &mut [u8], base: u64) {
        while self.sq[q].head != self.sq[q].tail {
            let s = self.sq[q];
            let Some(cmd) = rd(ram, base, s.base + 64 * s.head as u64, 64).map(|c| c.to_vec()) else { return };
            self.sq[q].head = (s.head + 1) % s.size;
            let cid = u16::from_le_bytes([cmd[2], cmd[3]]);
            let result = if q == 0 { self.admin(&cmd, ram, base) } else { self.io(&cmd, ram, base) };
            if let Some((status, dw0)) = result {
                self.complete(s.cq, q, cid, status, dw0, ram, base);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn complete(&mut self, cq: usize, sq: usize, cid: u16, status: u16, dw0: u32, ram: &mut [u8], base: u64) {
        let c = self.cq[cq];
        if !c.live {
            return;
        }
        let mut e = [0u8; 16];
        e[0..4].copy_from_slice(&dw0.to_le_bytes());
        e[8..10].copy_from_slice(&(self.sq[sq].head as u16).to_le_bytes());
        e[10..12].copy_from_slice(&(sq as u16).to_le_bytes());
        e[12..14].copy_from_slice(&cid.to_le_bytes());
        e[14..16].copy_from_slice(&((status << 1) | c.phase as u16).to_le_bytes());
        wr(ram, base, c.base + 16 * c.tail as u64, &e);
        let tail = (c.tail + 1) % c.size;
        self.cq[cq].tail = tail;
        if tail == 0 {
            self.cq[cq].phase = !c.phase;
        }
        if c.ien {
            self.irq_request = true;
        }
    }

    /// The guest buffer of a command's PRP1/PRP2 as (address, length) pieces.
    fn prps(&self, cmd: &[u8], len: usize, ram: &[u8], base: u64) -> Option<Vec<(u64, usize)>> {
        let prp1 = le64(cmd, 24);
        let prp2 = le64(cmd, 32);
        let first = (PAGE - (prp1 & (PAGE - 1))) as usize;
        let mut out = vec![(prp1, first.min(len))];
        let mut left = len.saturating_sub(first);
        if left == 0 {
            return Some(out);
        }
        if left <= PAGE as usize {
            out.push((prp2, left));
            return Some(out);
        }
        // PRP list (possibly chained through its last entry)
        let mut list = prp2;
        loop {
            let entries = ((PAGE - (list & (PAGE - 1))) / 8) as usize;
            for k in 0..entries {
                let e = le64(rd(ram, base, list + 8 * k as u64, 8)?, 0);
                if k == entries - 1 && left > PAGE as usize {
                    list = e;
                    break;
                }
                let n = left.min(PAGE as usize);
                out.push((e, n));
                left -= n;
                if left == 0 {
                    return Some(out);
                }
            }
        }
    }

    fn to_guest(&self, cmd: &[u8], data: &[u8], ram: &mut [u8], base: u64) -> bool {
        let Some(pieces) = self.prps(cmd, data.len(), ram, base) else { return false };
        let mut pos = 0;
        for (a, n) in pieces {
            if !wr(ram, base, a, &data[pos..pos + n]) {
                return false;
            }
            pos += n;
        }
        true
    }

    #[allow(clippy::wrong_self_convention)]
    fn from_guest(&self, cmd: &[u8], len: usize, ram: &[u8], base: u64) -> Option<Vec<u8>> {
        let mut data = Vec::with_capacity(len);
        for (a, n) in self.prps(cmd, len, ram, base)? {
            data.extend_from_slice(rd(ram, base, a, n)?);
        }
        Some(data)
    }

    fn blocks(&self) -> u64 {
        self.disk.as_ref().map_or(0, |d| d.sectors())
    }

    fn identify(&self, cns: u8, nsid: u32) -> Option<Vec<u8>> {
        let mut d = vec![0u8; 4096];
        let text = |d: &mut Vec<u8>, at: usize, len: usize, s: &str| {
            let mut b = s.as_bytes().to_vec();
            b.resize(len, b' ');
            d[at..at + len].copy_from_slice(&b);
        };
        match cns {
            1 => {
                d[0..2].copy_from_slice(&0xfffdu16.to_le_bytes()); // VID
                d[2..4].copy_from_slice(&0xfffdu16.to_le_bytes()); // SSVID
                text(&mut d, 4, 20, "SBCEMU0001");
                text(&mut d, 24, 40, "sbc-emu NVMe SSD");
                text(&mut d, 64, 8, "1.0");
                d[77] = MDTS;
                d[78..80].copy_from_slice(&1u16.to_le_bytes()); // CNTLID
                d[80..84].copy_from_slice(&0x0001_0400u32.to_le_bytes()); // VER
                d[258] = 3; // ACL
                d[259] = 3; // AERL
                d[260] = 0x02; // FRMW: one slot
                d[512] = 0x66; // SQES
                d[513] = 0x44; // CQES
                d[516..520].copy_from_slice(&1u32.to_le_bytes()); // NN
                d[520..522].copy_from_slice(&(0x4u16 | 0x8).to_le_bytes()); // ONCS: DSM, write zeroes
                d[525] = 1; // VWC
                let nqn = b"nqn.2026-11.org.sbc-emu:nvme:SBCEMU0001";
                d[768..768 + nqn.len()].copy_from_slice(nqn); // SUBNQN
                // one power state (25 W max)
                d[2048..2050].copy_from_slice(&2500u16.to_le_bytes());
            }
            0 if nsid == 1 => {
                let n = self.blocks();
                for o in [0, 8, 16] {
                    d[o..o + 8].copy_from_slice(&n.to_le_bytes()); // NSZE, NCAP, NUSE
                }
                d[25] = 0; // NLBAF: one format
                d[26] = 0; // FLBAS
                d[128..132].copy_from_slice(&(9u32 << 16).to_le_bytes()); // LBAF0: 512-byte blocks
            }
            0 => {}
            2 => {
                if nsid < 1 {
                    d[0..4].copy_from_slice(&1u32.to_le_bytes());
                }
            }
            3 if nsid == 1 => {
                // namespace identification descriptor: an EUI64
                d[0] = 1;
                d[1] = 8;
                d[4..12].copy_from_slice(&[0x02, 0x5b, 0xc0, 0x00, 0x00, 0x00, 0x00, 0x01]);
            }
            _ => return None,
        }
        Some(d)
    }

    /// An admin command: Some((status, dw0)) to complete it, None to hold it.
    fn admin(&mut self, cmd: &[u8], ram: &mut [u8], base: u64) -> Option<(u16, u32)> {
        let (op, nsid, cdw10, cdw11) = (cmd[0], le32(cmd, 4), le32(cmd, 40), le32(cmd, 44));
        const INVALID: u16 = 0x2;
        Some(match op {
            0x06 => match self.identify(cdw10 as u8, nsid) {
                Some(d) if self.to_guest(cmd, &d, ram, base) => (0, 0),
                Some(_) => (0x6, 0), // internal error: bad buffer
                None => (INVALID, 0),
            },
            0x05 => {
                // create I/O completion queue
                let (qid, size) = ((cdw10 & 0xffff) as usize, (cdw10 >> 16) + 1);
                if qid == 0 || qid >= MAX_QUEUES {
                    return Some((0x101, 0)); // invalid queue identifier
                }
                self.cq[qid] = Cq { base: le64(cmd, 24) & !0xfff, size, tail: 0, phase: true, ien: cdw11 & 2 != 0, live: true };
                (0, 0)
            }
            0x01 => {
                let (qid, size, cqid) = ((cdw10 & 0xffff) as usize, (cdw10 >> 16) + 1, (cdw11 >> 16) as usize);
                if qid == 0 || qid >= MAX_QUEUES || cqid >= MAX_QUEUES || !self.cq[cqid].live {
                    return Some((0x101, 0));
                }
                self.sq[qid] = Sq { base: le64(cmd, 24) & !0xfff, size, head: 0, tail: 0, cq: cqid, live: true };
                (0, 0)
            }
            0x00 => {
                if let Some(s) = self.sq.get_mut((cdw10 & 0xffff) as usize) {
                    s.live = false;
                }
                (0, 0)
            }
            0x04 => {
                if let Some(c) = self.cq.get_mut((cdw10 & 0xffff) as usize) {
                    c.live = false;
                }
                (0, 0)
            }
            0x09 => {
                let fid = cdw10 as u8;
                if fid == 7 {
                    // number of queues: as many as requested, up to 16 each way
                    let n = (MAX_QUEUES as u32 - 2).min(cdw11 & 0xffff).min(cdw11 >> 16);
                    (0, n | n << 16)
                } else {
                    self.features.insert(fid, cdw11);
                    (0, 0)
                }
            }
            0x0a => (0, self.features.get(&(cdw10 as u8)).copied().unwrap_or(0)),
            0x02 => {
                let dwords = (((le32(cmd, 44) & 0xffff) << 16) | (cdw10 >> 16)) as usize + 1;
                let d = vec![0u8; (dwords * 4).min(64 << 10)];
                if self.to_guest(cmd, &d, ram, base) { (0, 0) } else { (0x6, 0) }
            }
            0x0c => return None, // asynchronous event request: nothing to report
            0x08 => (0, 1),      // abort: not aborted
            _ => (0x1, 0),       // invalid opcode
        })
    }

    fn io(&mut self, cmd: &[u8], ram: &mut [u8], base: u64) -> Option<(u16, u32)> {
        let (op, nsid) = (cmd[0], le32(cmd, 4));
        if op != 0x00 && nsid != 1 {
            return Some((0xb, 0)); // invalid namespace
        }
        let slba = le64(cmd, 40);
        let nlb = (le32(cmd, 48) & 0xffff) as u64 + 1;
        let in_range = slba.checked_add(nlb).is_some_and(|e| e <= self.blocks()) && nlb <= (PAGE << MDTS) / 512;
        Some(match op {
            0x00 => (0, 0), // flush: writes reach the disk at once
            0x02 | 0x01 | 0x08 if !in_range => (0x80, 0), // LBA out of range
            0x02 => {
                let mut data = vec![0u8; (nlb * 512) as usize];
                if let Some(d) = self.disk.as_mut() {
                    d.read(slba, &mut data);
                }
                self.reads = self.reads.wrapping_add(1);
                if self.to_guest(cmd, &data, ram, base) { (0, 0) } else { (0x6, 0) }
            }
            0x01 => match self.from_guest(cmd, (nlb * 512) as usize, ram, base) {
                Some(data) => {
                    if let Some(d) = self.disk.as_mut() {
                        d.write(slba, &data);
                    }
                    self.writes = self.writes.wrapping_add(1);
                    (0, 0)
                }
                None => (0x6, 0),
            },
            0x08 => {
                if let Some(d) = self.disk.as_mut() {
                    d.write(slba, &vec![0u8; (nlb * 512) as usize]);
                }
                (0, 0)
            }
            0x09 => (0, 0), // dataset management (deallocate): accepted
            _ => (0x1, 0),
        })
    }
}

// Snapshots: what the guest wrote to the SSD, not the image.
emu_core::snap_struct!(Sq { base, size, head, tail, cq, live });
emu_core::snap_struct!(Cq { base, size, tail, phase, ien, live });

impl emu_core::snap::Snap for Nvme {
    fn save(&self, w: &mut emu_core::snap::W) {
        let Self { disk, cc, csts, aqa, asq, acq, intms, sq, cq, features, irq_request, reads, writes } = self;
        for v in [cc, csts, aqa, intms] { v.save(w); }
        for v in [asq, acq, reads, writes] { v.save(w); }
        sq.save(w);
        cq.save(w);
        features.save(w);
        irq_request.save(w);
        emu_core::snap::save_disk(w, disk.as_ref().map(|d| d.as_ref() as &dyn Disk));
    }
    fn load(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        let Self { disk, cc, csts, aqa, asq, acq, intms, sq, cq, features, irq_request, reads, writes } = self;
        for v in [cc, csts, aqa, intms] { v.load(r)?; }
        for v in [asq, acq, reads, writes] { v.load(r)?; }
        sq.load(r)?;
        cq.load(r)?;
        // what the guest could not have made: indexes and sizes the queues' arithmetic relies on
        if sq.iter().any(|q| q.cq >= MAX_QUEUES || q.live && (q.size == 0 || q.head >= q.size || q.tail >= q.size))
            || cq.iter().any(|q| q.live && (q.size == 0 || q.tail >= q.size))
        {
            return Err(emu_core::snap::SnapError("NVMe queues out of range".into()));
        }
        features.load(r)?;
        irq_request.load(r)?;
        emu_core::snap::load_disk(r, disk.as_mut().map(|d| d.as_mut() as &mut dyn Disk), "an NVMe SSD (--nvme)")
    }
}

#[cfg(test)]
mod tests {
    /// Queues a guest could not have made are refused, not divided by: a hand-made snapshot.
    #[test]
    fn a_snapshot_with_impossible_queues_is_refused() {
        use emu_core::snap::Snap;
        for (k, bad) in [Sq { live: true, size: 0, ..Default::default() }, Sq { live: true, size: 4, tail: 4, ..Default::default() }].into_iter().enumerate() {
            let mut n = Nvme::new(None);
            n.sq[1] = bad;
            let mut w = emu_core::snap::W::default();
            n.save(&mut w);
            assert!(Nvme::new(None).load(&mut emu_core::snap::R::new(&w.0)).is_err(), "case {k}");
        }
        let mut n = Nvme::new(None);
        n.cq[2] = Cq { live: true, size: 0, ..Default::default() };
        let mut w = emu_core::snap::W::default();
        n.save(&mut w);
        assert!(Nvme::new(None).load(&mut emu_core::snap::R::new(&w.0)).is_err());
    }

    use super::*;
    use emu_core::disk::MemDisk;

    const BASE: u64 = 0x4000_0000;

    fn put(ram: &mut [u8], a: u64, data: &[u8]) {
        let o = (a - BASE) as usize;
        ram[o..o + data.len()].copy_from_slice(data);
    }

    fn cmd(op: u8, cid: u16, nsid: u32, prp1: u64, cdw10: u32, cdw11: u32, cdw12: u32) -> [u8; 64] {
        let mut c = [0u8; 64];
        c[0] = op;
        c[2..4].copy_from_slice(&cid.to_le_bytes());
        c[4..8].copy_from_slice(&nsid.to_le_bytes());
        c[24..32].copy_from_slice(&prp1.to_le_bytes());
        c[40..44].copy_from_slice(&cdw10.to_le_bytes());
        c[44..48].copy_from_slice(&cdw11.to_le_bytes());
        c[48..52].copy_from_slice(&cdw12.to_le_bytes());
        c
    }

    #[test]
    fn admin_identify_io_queues_and_read_write() {
        let mut ram = vec![0u8; 0x20000];
        let mut disk = MemDisk::new(64);
        disk.0[512..516].copy_from_slice(b"NVME");
        let mut n = Nvme::new(Some(Box::new(disk)));
        n.write(0x24, (7 << 16) | 7, &mut ram, BASE);
        n.write(0x28, (BASE + 0x1000) as u32, &mut ram, BASE);
        n.write(0x30, (BASE + 0x2000) as u32, &mut ram, BASE);
        n.write(0x14, 1, &mut ram, BASE);
        assert_eq!(n.read(0x1c) & 1, 1);
        // identify controller into 0x4000
        put(&mut ram, BASE + 0x1000, &cmd(0x06, 7, 0, BASE + 0x4000, 1, 0, 0));
        n.write(0x1000, 1, &mut ram, BASE);
        assert!(n.irq_request);
        assert_eq!(&ram[0x4000 + 24..0x4000 + 31], b"sbc-emu");
        let cqe = &ram[0x2000..0x2010];
        assert_eq!(u16::from_le_bytes([cqe[12], cqe[13]]), 7);
        assert_eq!(cqe[14] & 1, 1, "phase");
        // I/O CQ 1 at 0x6000, SQ 1 at 0x7000
        put(&mut ram, BASE + 0x1040, &cmd(0x05, 8, 0, BASE + 0x6000, (3 << 16) | 1, 3, 0));
        put(&mut ram, BASE + 0x1080, &cmd(0x01, 9, 0, BASE + 0x7000, (3 << 16) | 1, 1 << 16, 0));
        n.write(0x1000, 3, &mut ram, BASE);
        // read block 1 into 0x8000, then write it to block 5 and read back
        put(&mut ram, BASE + 0x7000, &cmd(0x02, 1, 1, BASE + 0x8000, 1, 0, 0));
        n.write(0x1008, 1, &mut ram, BASE);
        assert_eq!(&ram[0x8000..0x8004], b"NVME");
        put(&mut ram, BASE + 0x7040, &cmd(0x01, 2, 1, BASE + 0x8000, 5, 0, 0));
        put(&mut ram, BASE + 0x7080, &cmd(0x02, 3, 1, BASE + 0x9000, 5, 0, 0));
        n.write(0x1008, 3, &mut ram, BASE);
        assert_eq!(&ram[0x9000..0x9004], b"NVME");
        assert_eq!(u16::from_le_bytes([ram[0x6000 + 32 + 12], ram[0x6000 + 32 + 13]]), 3);
        // out of range
        put(&mut ram, BASE + 0x70c0, &cmd(0x02, 4, 1, BASE + 0x9000, 100, 0, 0));
        n.write(0x1008, 0, &mut ram, BASE);
        let st = u16::from_le_bytes([ram[0x6000 + 48 + 14], ram[0x6000 + 48 + 15]]) >> 1;
        assert_eq!(st, 0x80);
    }
}
