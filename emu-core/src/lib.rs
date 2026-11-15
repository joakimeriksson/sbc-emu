//! The contract between a CPU core and the SoC around it (as in esp32sim's emu-core, with
//! 64-bit physical addresses): `Bus` (memory), `Core` (what a machine drives), `Trap` (why a
//! step stopped). No dependencies, no chip knowledge.

pub mod snap;
pub mod fdt;
mod stop;
pub use stop::Stop;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// no memory / device at this address
    Unmapped,
    /// address exists but this access is not allowed
    Prohibited,
}

/// Physical memory as a core sees it. All accesses are little-endian.
pub trait Bus {
    fn read8(&mut self, addr: u64) -> Result<u8, Fault>;
    fn read16(&mut self, addr: u64) -> Result<u16, Fault>;
    fn read32(&mut self, addr: u64) -> Result<u32, Fault>;
    fn read64(&mut self, addr: u64) -> Result<u64, Fault>;
    fn write8(&mut self, addr: u64, v: u8) -> Result<(), Fault>;
    fn write16(&mut self, addr: u64, v: u16) -> Result<(), Fault>;
    fn write32(&mut self, addr: u64, v: u32) -> Result<(), Fault>;
    fn write64(&mut self, addr: u64, v: u64) -> Result<(), Fault>;
    /// Optional CPU-interface devices addressed through architectural system registers.
    /// Default buses have no such devices. The CPU checks access permissions first.
    fn read_sysreg(&mut self, _key: u32) -> Option<u64> { None }
    fn write_sysreg(&mut self, _key: u32, _value: u64) -> bool { false }
    /// Instruction fetch (4-byte aligned).
    fn fetch(&mut self, pc: u64) -> Result<u32, Fault> {
        self.read32(pc)
    }
}

/// Simple flat RAM for unit tests and plain memory regions.
pub struct FlatRam {
    pub base: u64,
    pub mem: Vec<u8>,
}

impl FlatRam {
    pub fn new(base: u64, size: usize) -> Self {
        FlatRam {
            base,
            mem: vec![0; size],
        }
    }
    #[inline]
    fn off(&self, addr: u64, n: usize) -> Result<usize, Fault> {
        let o = addr.wrapping_sub(self.base);
        if o.checked_add(n as u64)
            .is_some_and(|end| end <= self.mem.len() as u64)
        {
            Ok(o as usize)
        } else {
            Err(Fault::Unmapped)
        }
    }
    pub fn load(&mut self, addr: u64, bytes: &[u8]) -> Result<(), Fault> {
        let o = self.off(addr, bytes.len())?;
        self.mem[o..o + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }
}

macro_rules! rw {
    ($r:ident, $w:ident, $t:ty, $n:expr) => {
        fn $r(&mut self, a: u64) -> Result<$t, Fault> {
            let o = self.off(a, $n)?;
            Ok(<$t>::from_le_bytes(self.mem[o..o + $n].try_into().unwrap()))
        }
        fn $w(&mut self, a: u64, v: $t) -> Result<(), Fault> {
            let o = self.off(a, $n)?;
            self.mem[o..o + $n].copy_from_slice(&v.to_le_bytes());
            Ok(())
        }
    };
}

impl Bus for FlatRam {
    rw!(read8, write8, u8, 1);
    rw!(read16, write16, u16, 2);
    rw!(read32, write32, u32, 4);
    rw!(read64, write64, u64, 8);
}

/// Why `step` returned something other than `Ok`. Architectural exceptions taken by the guest
/// are reported (the pc already points at the vector) so a machine can count or stop on them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trap {
    /// architectural synchronous exception taken: (exception class, target EL)
    Exception(u32, u8),
    /// interrupt taken: (0 = IRQ, 1 = FIQ, target EL)
    Interrupt(u8, u8),
    /// instruction not implemented by the emulator (pc, raw word)
    Unimplemented(u64, u32),
    /// `brk #imm` while BRK is routed to the host (tests, bare-metal fixtures)
    Brk(u16),
    /// `hlt #imm` (semihosting-style host stop)
    Hlt(u16),
    /// core is waiting for an interrupt (`wfi`) or event (`wfe`)
    Wait,
    /// `smc`/`hvc` handled by the host as firmware (PSCI); the instruction has retired
    HostCall(u16),
}

/// A CPU core as the machine sees it.
pub trait Core {
    fn step(&mut self, bus: &mut dyn Bus) -> Result<(), Trap>;
    fn pc(&self) -> u64;
    /// retired instruction count
    fn instret(&self) -> u64;
    /// interrupt inputs (level): IRQ and FIQ
    fn set_irq(&mut self, irq: bool, fiq: bool);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounds_cannot_wrap() {
        let mut r = FlatRam::new(0x1000, 16);
        assert_eq!(r.read32(0x0ffe), Err(Fault::Unmapped));
        assert_eq!(r.read64(0x1008), Ok(0));
        assert_eq!(r.read64(0x1009), Err(Fault::Unmapped));
        assert_eq!(r.read8(u64::MAX), Err(Fault::Unmapped));
    }
}
