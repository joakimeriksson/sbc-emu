//! PCI Express functions behind a host bridge: a configuration space with read-only, writable
//! and write-one-to-clear bits and BAR sizing, the trait a card implements, and the bus-master
//! side (DMA and MSI writes) the host bridge gives it.

/// One function's 4 KiB configuration space.
pub struct Config {
    pub data: Vec<u8>,
    /// bits software may write
    wmask: Vec<u8>,
    /// bits a 1 clears
    w1c: Vec<u8>,
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

/// Command register bits.
pub const CMD_MEMORY: u16 = 1 << 1;
pub const CMD_MASTER: u16 = 1 << 2;

impl Config {
    pub fn new() -> Self {
        Self { data: vec![0; 4096], wmask: vec![0; 4096], w1c: vec![0; 4096] }
    }

    /// A type 0 (endpoint) header.
    pub fn endpoint(vendor: u16, device: u16, class: u32, revision: u8) -> Self {
        let mut c = Self::new();
        c.set16(0x00, vendor);
        c.set16(0x02, device);
        c.set32(0x08, class << 8 | revision as u32);
        // command: I/O, memory, bus master, parity, SERR, INTx disable; status: error bits
        c.writable(0x04, 2, 0x0547);
        c.clearable(0x06, 2, 0xf900);
        c.writable(0x0c, 1, 0xff); // cache line size
        c.writable(0x0d, 1, 0xff); // latency timer
        c.writable(0x3c, 1, 0xff); // interrupt line
        c
    }

    pub fn get8(&self, o: usize) -> u8 {
        self.data[o]
    }
    pub fn get16(&self, o: usize) -> u16 {
        u16::from_le_bytes([self.data[o], self.data[o + 1]])
    }
    pub fn get32(&self, o: usize) -> u32 {
        u32::from_le_bytes(self.data[o..o + 4].try_into().unwrap())
    }
    pub fn set8(&mut self, o: usize, v: u8) {
        self.data[o] = v;
    }
    pub fn set16(&mut self, o: usize, v: u16) {
        self.data[o..o + 2].copy_from_slice(&v.to_le_bytes());
    }
    pub fn set32(&mut self, o: usize, v: u32) {
        self.data[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// Make `bits` of the `len`-byte register at `o` writable.
    pub fn writable(&mut self, o: usize, len: usize, bits: u32) {
        for (k, b) in bits.to_le_bytes()[..len].iter().enumerate() {
            self.wmask[o + k] |= b;
        }
    }

    /// Make `bits` of the `len`-byte register at `o` write-one-to-clear.
    pub fn clearable(&mut self, o: usize, len: usize, bits: u32) {
        for (k, b) in bits.to_le_bytes()[..len].iter().enumerate() {
            self.w1c[o + k] |= b;
        }
    }

    /// A memory BAR at `o` of `size` bytes (a power of two): its address bits writable, the
    /// rest reading as zero, as the sizing write of all ones finds them. 64-bit BARs take
    /// the next register too.
    pub fn memory_bar(&mut self, o: usize, size: u64, bits64: bool, prefetch: bool) {
        let mask = !(size - 1);
        self.set32(o, u32::from(bits64) << 2 | u32::from(prefetch) << 3);
        self.writable(o, 4, mask as u32);
        if bits64 {
            self.writable(o + 4, 4, (mask >> 32) as u32);
        }
    }

    /// A capability with `id` at `o` linked to `next`.
    pub fn capability(&mut self, o: usize, id: u8, next: u8) {
        self.data[o] = id;
        self.data[o + 1] = next;
    }

    /// The 32-bit register at `o` (aligned down).
    pub fn read(&self, o: usize) -> u32 {
        self.get32(o & 0xffc)
    }

    /// A write of `len` bytes (1, 2 or 4) at `o`.
    pub fn write(&mut self, o: usize, value: u32, len: usize) {
        for (k, b) in value.to_le_bytes()[..len].iter().enumerate() {
            let i = o + k;
            if i >= self.data.len() {
                break;
            }
            let old = self.data[i];
            self.data[i] = (old & !self.wmask[i]) | (b & self.wmask[i]);
            self.data[i] &= !(b & self.w1c[i]);
        }
    }

    pub fn command(&self) -> u16 {
        self.get16(0x04)
    }

    /// The address a memory BAR at `o` decodes (with its upper half when 64-bit).
    pub fn bar_address(&self, o: usize) -> u64 {
        let lo = self.get32(o);
        let hi = if lo & 0b110 == 0b100 { self.get32(o + 4) as u64 } else { 0 };
        hi << 32 | (lo & !0xf) as u64
    }
}

/// What a bus-mastering function reaches through its host bridge: memory (DMA) and, through
/// a write to the MSI address, interrupts. False when nothing answers there.
pub trait Master {
    fn read(&mut self, addr: u64, buf: &mut [u8]) -> bool;
    fn write(&mut self, addr: u64, data: &[u8]) -> bool;
    /// The time now (ns), for work the function finishes later.
    fn now(&self) -> u64 {
        0
    }
    /// Guest RAM directly, for a function that works on descriptors in place (None: only
    /// `read` and `write`).
    fn ram(&mut self) -> Option<crate::dma::Dma<'_>> {
        None
    }
}

/// A PCI Express function (device 0 on the bus below a root port).
pub trait Function: Send {
    fn config(&mut self) -> &mut Config;
    /// A configuration write went through (the function reacts to its own registers).
    fn config_written(&mut self, _offset: usize, _master: &mut dyn Master) {}
    /// The BAR (0..6) and offset an address of the memory space decodes to, if any.
    fn decode(&mut self, addr: u64) -> Option<(usize, u64)> {
        if self.config().command() & CMD_MEMORY == 0 {
            return None;
        }
        let mut bar = 0;
        while bar < 6 {
            let o = 0x10 + 4 * bar;
            let c = self.config();
            let lo = c.get32(o);
            let bits64 = lo & 0b111 == 0b100;
            let size = self.bar_size(bar);
            let base = self.config().bar_address(o);
            if size != 0 && base != 0 && (base..base + size).contains(&addr) {
                return Some((bar, addr - base));
            }
            bar += if bits64 { 2 } else { 1 };
        }
        None
    }
    fn bar_size(&self, bar: usize) -> u64;
    fn bar_read(&mut self, bar: usize, offset: u64, len: usize, master: &mut dyn Master) -> u64;
    fn bar_write(&mut self, bar: usize, offset: u64, value: u64, len: usize, master: &mut dyn Master);
    /// Time-driven work (`now` in ns): the function's own deadlines.
    fn tick(&mut self, _now: u64, _master: &mut dyn Master) {}
    fn next_event(&self) -> Option<u64> {
        None
    }
    fn as_any(&mut self) -> &mut dyn std::any::Any;
}

/// The MSI capability at `o`: its address and data when the function may signal one.
pub fn msi_target(c: &Config, o: usize) -> Option<(u64, u32)> {
    let control = c.get16(o + 2);
    if control & 1 == 0 {
        return None;
    }
    let bits64 = control & 0x80 != 0;
    let addr = c.get32(o + 4) as u64 | if bits64 { (c.get32(o + 8) as u64) << 32 } else { 0 };
    let data = c.get16(o + if bits64 { 12 } else { 8 }) as u32;
    Some((addr, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_sizing_reads_back_the_size() {
        let mut c = Config::endpoint(0x14e4, 0x43a3, 0x028000, 8);
        c.memory_bar(0x10, 0x8000, true, false);
        c.write(0x10, 0xffff_ffff, 4);
        c.write(0x14, 0xffff_ffff, 4);
        assert_eq!(c.get32(0x10), 0xffff_8004);
        assert_eq!(c.get32(0x14), 0xffff_ffff);
        c.write(0x10, 0x4000_0000, 4);
        c.write(0x14, 0, 4);
        assert_eq!(c.bar_address(0x10), 0x4000_0000);
        // vendor ID is read-only, status bits clear on one
        c.write(0x00, 0, 4);
        assert_eq!(c.get16(0), 0x14e4);
        c.set16(0x06, 0x8010);
        c.write(0x04, 0x8000_0006, 4);
        assert_eq!(c.get16(0x06), 0x0010);
        assert_eq!(c.command(), 6);
    }
}
