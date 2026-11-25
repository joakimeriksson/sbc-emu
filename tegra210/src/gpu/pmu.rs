//! The power management unit's falcon, at a high level: it runs NVIDIA's firmware, which is not
//! executed here. The first start is the secure loader, which halts and reports success; the next
//! is the real-time OS, which announces its queues and answers the two commands nouveau sends it
//! (set up the write-protected region, start another falcon).
pub const BASE: u32 = 0x10_a000;
const DMEM: usize = 0xa000;

const QUEUES: usize = 0x9000;
const QUEUE: usize = 0x100;

pub struct Pmu {
    pub dmem: Vec<u8>,
    regs: std::collections::HashMap<u32, u32>,
    /// the data ports: offset, increment after a write, after a read
    ports: [(u32, bool, bool); 8],
    /// a boot vector was written: the next start is the secure loader's
    loader: bool,
    running: bool,
    halted: bool,
    bound: bool,
    message: u32,
}

impl Default for Pmu {
    fn default() -> Self {
        Self { dmem: vec![0; DMEM], regs: Default::default(), ports: [(0, false, false); 8], loader: false, running: false, halted: true, bound: false, message: 0 }
    }
}

impl Pmu {
    pub fn irq(&self) -> bool {
        let r = |o| self.regs.get(&o).copied().unwrap_or(0);
        r(0x008) & 0x40 != 0 && self.running
    }

    fn r(&self, o: u32) -> u32 {
        self.regs.get(&o).copied().unwrap_or(0)
    }

    pub fn read(&mut self, o: u32) -> u32 {
        match o {
            0x100 => (self.halted as u32) << 4,
            0x108 => (DMEM as u32 >> 8) << 9 | 0xc0, // 48 KiB of code, 40 of data
            0x12c => 4 << 12 | 1 << 8 | 2 << 4 | 5,
            0x0dc | 0x20c => if self.bound { 5 << 12 } else { 0 },
            0x10c => self.r(o) & !6, // memories scrubbed
            // a data port's control: where it is and how it moves
            0x1c0..=0x1ff if o & 7 == 0 => {
                let (a, w, r) = self.ports[(o as usize - 0x1c0) / 8];
                a | (w as u32) << 24 | (r as u32) << 25
            }
            0x1c4..=0x1ff if o & 7 == 4 => {
                let p = (o as usize - 0x1c4) / 8;
                let a = self.ports[p].0 as usize & 0xfffc;
                if self.ports[p].2 {
                    self.ports[p].0 += 4;
                }
                self.dmem.get(a..a + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
            }
            _ => self.r(o),
        }
    }

    pub fn write(&mut self, o: u32, v: u32) {
        match o {
            0x004 => {
                let s = self.r(0x008) & !v;
                self.regs.insert(0x008, s);
            }
            0x054 | 0x480 => {
                self.bound = true;
                let s = self.r(0x008) | 8;
                self.regs.insert(0x008, s);
                self.regs.insert(o, v);
            }
            0x058 if v & 2 != 0 => self.bound = false,
            0x104 => {
                self.loader = true;
                self.regs.insert(o, v);
            }
            0x100 | 0x130 if v & 2 != 0 => self.start(),
            0x3c0 if v & 1 != 0 => {
                // engine reset
                let keep = std::mem::take(&mut self.dmem);
                *self = Self { dmem: keep, ..Self::default() };
            }
            0x1c0..=0x1ff => {
                let p = (o as usize - 0x1c0) / 8;
                if o & 7 == 0 {
                    self.ports[p] = (v & 0xffff, v >> 24 & 1 != 0, v >> 25 & 1 != 0);
                } else {
                    let a = self.ports[p].0 as usize & 0xfffc;
                    if let Some(b) = self.dmem.get_mut(a..a + 4) {
                        b.copy_from_slice(&v.to_le_bytes());
                    }
                    if self.ports[p].1 {
                        self.ports[p].0 += 4;
                    }
                }
            }
            0x4a0 | 0x4a4 => {
                self.regs.insert(o, v);
                self.commands((o - 0x4a0) as usize / 4);
            }
            _ => {
                self.regs.insert(o, v);
            }
        }
    }

    fn start(&mut self) {
        if std::mem::take(&mut self.loader) {
            self.halted = true;
            self.regs.insert(0x040, 0); // no error
            return;
        }
        self.halted = false;
        self.running = true;
        self.regs.insert(0x01c, 0x40); // its software interrupt goes to the host
        let queue = |k: usize| (QUEUES + k * QUEUE) as u16;
        for k in 0..2u32 {
            self.regs.insert(0x4a0 + 4 * k, queue(k as usize) as u32);
            self.regs.insert(0x4b0 + 4 * k, queue(k as usize) as u32);
        }
        self.message = queue(4) as u32;
        self.regs.insert(0x4cc, self.message);
        let mut m = vec![0x07, 42, 0, 0, 0, 0, 0, 0];
        for k in [0usize, 1, 2, 3, 4] {
            m.extend((QUEUE as u16).to_le_bytes());
            m.extend(queue(k).to_le_bytes());
            m.extend([k as u8, 0]);
        }
        m.extend([0; 4]);
        self.post(&m);
    }

    /// Put a message into the message queue and raise the interrupt.
    fn post(&mut self, m: &[u8]) {
        let n = m.len().next_multiple_of(4);
        let start = (QUEUES + 4 * QUEUE) as u32;
        if self.message as usize + n + 4 > start as usize + QUEUE {
            self.message = start; // the reader follows: head < its position
        }
        let a = self.message as usize;
        self.dmem[a..a + m.len()].copy_from_slice(m);
        self.message += n as u32;
        self.regs.insert(0x4c8, self.message);
        let s = self.r(0x008) | 0x40;
        self.regs.insert(0x008, s);
    }

    fn commands(&mut self, q: usize) {
        let (head, mut tail) = (self.r(0x4a0 + 4 * q as u32), self.r(0x4b0 + 4 * q as u32));
        for _ in 0..64 {
            if tail == head {
                break;
            }
            let a = tail as usize;
            let Some(hdr) = self.dmem.get(a..a + 8).map(|b| b.to_vec()) else { break };
            let (unit, size, seq, kind) = (hdr[0], hdr[1] as usize, hdr[3], hdr[4]);
            if unit == 0 {
                tail = (QUEUES + q * QUEUE) as u32; // rewind
                continue;
            }
            let data = self.dmem.get(a..a + size).map(|b| b.to_vec()).unwrap_or_default();
            let falcon = data.get(12..16).map_or([0; 4], |b| b.try_into().unwrap());
            // the region is ready (no error); the falcon is started: its number
            let result = if kind == 1 { falcon } else { [0; 4] };
            self.post(&[&[unit, 12, 0, seq, kind, 0, 0, 0][..], &result].concat());
            tail += size.next_multiple_of(4) as u32;
        }
        self.regs.insert(0x4b0 + 4 * q as u32, tail);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loader_then_firmware() {
        let mut p = Pmu::default();
        p.write(0x104, 0x100);
        p.write(0x040, 0xcafe_beef);
        p.write(0x100, 2);
        assert_eq!((p.read(0x100) & 0x10, p.read(0x040), p.irq()), (0x10, 0, false));
        p.write(0x100, 2);
        assert!(p.irq());
        let tail = p.read(0x4cc);
        p.write(0x1c0, 1 << 25 | tail);
        assert_eq!(p.read(0x1c4), 0x2a07);
        p.write(0x004, 0x40);
        assert!(!p.irq());
        // start falcon 2
        let head = p.read(0x4a0);
        p.write(0x1c0, 1 << 24 | head);
        for w in [0x0a | 16 << 8 | 5 << 24u32, 1, 0, 2] {
            p.write(0x1c4, w);
        }
        p.write(0x4a0, head + 16);
        assert!(p.irq());
        let a = p.read(0x4c8) as usize - 12;
        assert_eq!(p.dmem[a..a + 12], [0x0a, 12, 0, 5, 1, 0, 0, 0, 2, 0, 0, 0]);
        assert_eq!(p.read(0x4b0), head + 16);
    }
}
