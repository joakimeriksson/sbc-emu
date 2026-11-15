use crate::cpu::{Cpu, MODE_USR};
use emu_core::Bus;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    Read,
    Write,
    Execute,
}

impl Cpu {
    fn domain_access(&self, domain: u32, page: bool) -> Result<bool, u32> {
        match (self.dacr >> (domain * 2)) & 3 {
            1 => Ok(false), // client: check descriptor permissions
            3 => Ok(true),  // manager: bypass AP, but not XN/PXN
            _ => Err((if page { 0x0b } else { 0x09 }) | domain << 4),
        }
    }

    fn permission(ap: u32, privileged: bool, write: bool) -> bool {
        match ap {
            1 => privileged,
            2 => privileged || !write,
            3 => true,
            5 => privileged && !write,
            6 | 7 => !write,
            _ => false,
        }
    }

    /// Translate one virtual address using ARMv7 short descriptors.
    /// The returned fault is the architectural FSR value before the write bit is added.
    fn translate(&self, bus: &mut dyn Bus, va: u32, access: Access) -> Result<u32, (u32, u32)> {
        if self.sctlr & 1 == 0 {
            return Ok(va);
        }
        let fault = |status| Err((va, status));
        let n = self.ttbcr & 7;
        let boundary = (0x1_0000_0000u64 >> n) as u32;
        let use_ttbr1 = n != 0 && va >= boundary;
        if use_ttbr1 && self.ttbcr & (1 << 5) != 0 || !use_ttbr1 && self.ttbcr & (1 << 4) != 0 {
            return fault(0x05);
        }
        let table = if use_ttbr1 {
            self.ttbr1 & 0xffff_c000
        } else {
            self.ttbr0 & (0xffff_c000 << n)
        };
        let l1_address = table as u64 + ((va >> 20) as u64) * 4;
        let l1 = bus.read32(l1_address).map_err(|_| (va, 0x0c))?;
        let domain = (l1 >> 5) & 0xf;
        let privileged = self.cpsr & 0x1f != MODE_USR;
        let write = access == Access::Write;

        if l1 & 3 == 0 {
            return fault(0x05);
        }
        if l1 & 2 != 0 {
            // Section. Bit zero is PXN; supersections with extended physical addresses
            // are outside the H3's 32-bit memory map and are not implemented.
            if l1 & (1 << 18) != 0 {
                return fault(0x05);
            }
            let manager = self
                .domain_access(domain, false)
                .map_err(|status| (va, status))?;
            let ap = (l1 >> 10) & 3 | (l1 >> 13) & 4;
            let execute_never =
                access == Access::Execute && (l1 & (1 << 4) != 0 || privileged && l1 & 1 != 0);
            if execute_never || !manager && !Self::permission(ap, privileged, write) {
                return fault(0x0d | domain << 4);
            }
            return Ok((l1 & 0xfff0_0000) | (va & 0x000f_ffff));
        }

        // Coarse second-level table.
        let manager = self
            .domain_access(domain, true)
            .map_err(|status| (va, status))?;
        let l2_address = (l1 & 0xffff_fc00) as u64 + ((va >> 12) & 0xff) as u64 * 4;
        let l2 = bus.read32(l2_address).map_err(|_| (va, 0x0e))?;
        if l2 & 3 == 0 {
            return fault(0x07 | domain << 4);
        }
        let small = l2 & 2 != 0;
        let (physical, ap, execute_never) = if small {
            (
                (l2 & 0xffff_f000) | (va & 0xfff),
                (l2 >> 4) & 3 | (l2 >> 7) & 4,
                l2 & 1 != 0,
            )
        } else {
            (
                (l2 & 0xffff_0000) | (va & 0xffff),
                (l2 >> 4) & 3 | (l2 >> 7) & 4,
                l2 & (1 << 15) != 0,
            )
        };
        let privileged_execute_never = l1 & (1 << 2) != 0;
        if access == Access::Execute && (execute_never || privileged && privileged_execute_never)
            || !manager && !Self::permission(ap, privileged, write)
        {
            return fault(0x0f | domain << 4);
        }
        Ok(physical)
    }

    pub(crate) fn mem_read8(
        &self,
        bus: &mut dyn Bus,
        va: u32,
        access: Access,
    ) -> Result<u8, (u32, u32)> {
        let physical = self.translate(bus, va, access)?;
        bus.read8(physical as u64).map_err(|_| (va, 0x08))
    }

    fn mem_read_array<const N: usize>(
        &self,
        bus: &mut dyn Bus,
        va: u32,
        access: Access,
    ) -> Result<[u8; N], (u32, u32)> {
        let mut bytes = [0; N];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = self.mem_read8(bus, va.wrapping_add(index as u32), access)?;
        }
        Ok(bytes)
    }

    pub(crate) fn mem_read16(&self, bus: &mut dyn Bus, va: u32) -> Result<u16, (u32, u32)> {
        if self.sctlr & 1 == 0 {
            return bus.read16(va as u64).map_err(|_| (va, 0x08));
        }
        self.mem_read_array(bus, va, Access::Read)
            .map(u16::from_le_bytes)
    }

    pub(crate) fn mem_read32(
        &self,
        bus: &mut dyn Bus,
        va: u32,
        access: Access,
    ) -> Result<u32, (u32, u32)> {
        if self.sctlr & 1 == 0 {
            return bus.read32(va as u64).map_err(|_| (va, 0x08));
        }
        self.mem_read_array(bus, va, access).map(u32::from_le_bytes)
    }

    pub(crate) fn mem_write8(
        &self,
        bus: &mut dyn Bus,
        va: u32,
        value: u8,
    ) -> Result<(), (u32, u32)> {
        let physical = self.translate(bus, va, Access::Write)?;
        bus.write8(physical as u64, value).map_err(|_| (va, 0x08))
    }

    fn mem_write_array(&self, bus: &mut dyn Bus, va: u32, bytes: &[u8]) -> Result<(), (u32, u32)> {
        for (index, byte) in bytes.iter().enumerate() {
            self.mem_write8(bus, va.wrapping_add(index as u32), *byte)?;
        }
        Ok(())
    }

    pub(crate) fn mem_write16(
        &self,
        bus: &mut dyn Bus,
        va: u32,
        value: u16,
    ) -> Result<(), (u32, u32)> {
        if self.sctlr & 1 == 0 {
            return bus.write16(va as u64, value).map_err(|_| (va, 0x08));
        }
        self.mem_write_array(bus, va, &value.to_le_bytes())
    }

    pub(crate) fn mem_write32(
        &self,
        bus: &mut dyn Bus,
        va: u32,
        value: u32,
    ) -> Result<(), (u32, u32)> {
        if self.sctlr & 1 == 0 {
            return bus.write32(va as u64, value).map_err(|_| (va, 0x08));
        }
        self.mem_write_array(bus, va, &value.to_le_bytes())
    }
}
