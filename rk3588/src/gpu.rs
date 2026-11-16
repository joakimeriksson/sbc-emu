//! Mali-G610 control registers and host-modeled CSF management.
//! MCU instructions and graphics queue execution remain unsupported.
use crate::gpu_mmu::AddressSpace;
#[derive(Default)]
pub struct Gpu {
    raw: [u32; 3],
    firmware: crate::gpu_fw::Firmware,
    mask: [u32; 3],
    ready: [u64; 3],
    regs: std::collections::BTreeMap<u64, u32>,
    pub spaces: [AddressSpace; 16],
}
impl Gpu {
    pub fn irq(&self, line: usize) -> bool {
        self.raw[line] & self.mask[line] != 0
    }
    pub fn read(&self, off: u64) -> u32 {
        for (i, base) in [0x1000, 0x2000, 0x20].into_iter().enumerate() {
            if off == base {
                return self.raw[i];
            }
            if off == base + 8 {
                return self.mask[i];
            }
            if off == base + 12 {
                return self.raw[i] & self.mask[i];
            }
        }
        for (i, base) in [0x140, 0x150, 0x160].into_iter().enumerate() {
            if off == base {
                return self.ready[i] as u32;
            }
            if off == base + 4 {
                return (self.ready[i] >> 32) as u32;
            }
        }
        match off {
            0 => 0xa867_0000,
            4 => 6,
            0x14 => 0x2830,
            0x18 => 0xffff,
            0x100 => 0xf,
            0x110 | 0x120 => 1,
            0x300 => 1, // ACE-lite
            _ => *self.regs.get(&off).unwrap_or(&0),
        }
    }
    pub fn write_memory(&mut self, off: u64, v: u32, ram: &mut [u8]) {
        self.write(off, v);
        if off == 0x700 && v != 0 {
            if self.firmware.boot(&self.spaces[0], ram).is_ok() {
                self.regs.insert(0x704, 1);
                self.raw[0] |= 1 << 31;
            }
        } else if off == 0x700 {
            self.firmware.running = false;
        } else if off == 0x80000 {
            match self.firmware.doorbell(&self.spaces[0], ram) {
                Ok((irq, halt)) => {
                    if irq {
                        self.raw[0] |= 1 << 31;
                    }
                    if halt {
                        self.regs.insert(0x704, 2);
                    }
                }
                Err(_) => {
                    self.firmware.running = false;
                    self.regs.insert(0x704, 3);
                    self.raw[2] |= 1 << 19;
                }
            }
        }
    }
    pub fn write(&mut self, off: u64, v: u32) {
        for (i, base) in [0x1000, 0x2000, 0x20].into_iter().enumerate() {
            if off == base {
                self.raw[i] |= v;
                return;
            }
            if off == base + 4 {
                self.raw[i] &= !v;
                return;
            }
            if off == base + 8 {
                self.mask[i] = v;
                return;
            }
            if off == base + 12 {
                return;
            }
        }
        for (i, base) in [0x180, 0x190, 0x1a0].into_iter().enumerate() {
            for (b, on) in [(base, true), (base + 0x40, false)] {
                if off == b || off == b + 4 {
                    let bits = (v as u64) << if off == b { 0 } else { 32 };
                    let present = if i == 0 { 15 } else { 1 };
                    if on {
                        self.ready[i] |= bits & present;
                    } else {
                        self.ready[i] &= !bits;
                    }
                    self.raw[2] |= (1 << 9) | (1 << 10);
                    return;
                }
            }
        }
        match off {
            0x30 if v == 0x101 || v == 0x201 => {
                *self = Self::default();
                self.raw[2] = 1 << 8;
            }
            0x30 if v & 255 == 4 => self.raw[2] |= 1 << 17,
            0x700 => {
                // Never acknowledge firmware initialization: no MCU executes yet.
                self.regs.insert(0x704, if v == 0 { 0 } else { 3 });
                self.raw[2] |= 1 << 19;
            }
            0x2400..=0x27ff => {
                let n = ((off - 0x2400) / 64) as usize;
                let reg = (off - 0x2400) % 64;
                if reg == 0x18 {
                    if v == 1 {
                        let base = 0x2400 + n as u64 * 64;
                        self.spaces[n] = AddressSpace {
                            transtab: self.read(base) as u64 | ((self.read(base + 4) as u64) << 32),
                            transcfg: self.read(base + 48) as u64
                                | ((self.read(base + 52) as u64) << 32),
                        };
                    }
                } else if matches!(reg, 0 | 4 | 8 | 12 | 16 | 20 | 48 | 52) {
                    self.regs.insert(off, v);
                }
            }
            0x304 => {
                self.regs.insert(off, v);
            }
            _ => {}
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn linux_reset_power_and_mmu_update_sequence() {
        let mut g = Gpu::default();
        g.write(0x30, 0x101);
        assert_eq!(g.read(0x20), 256);
        g.write(0x28, 256);
        assert!(g.irq(2));
        g.write(0x24, 256);
        assert!(!g.irq(2));
        g.write(0x1a0, 1);
        assert_eq!(g.read(0x160), 1);
        g.write(0x180, u32::MAX);
        assert_eq!(g.read(0x140), 15);
        g.write(0x2400, 0x123000);
        g.write(0x2430, 6 | (7 << 6));
        assert_eq!(g.spaces[0].transtab, 0);
        g.write(0x2418, 1);
        assert_eq!(g.spaces[0].transtab, 0x123000);
        g.write(0x700, 2);
        assert_eq!(g.read(0x704), 3);
    }
}
