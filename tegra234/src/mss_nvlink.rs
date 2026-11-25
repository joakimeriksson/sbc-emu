//! Tegra234's internal GPU-to-memory links. DMA already shares the CPU's RAM,
//! so the credit kick and snoop mode need no separate transport scheduling.

const BASES: [u64; 8] = [
    0x01f2_0000,
    0x01f4_0000,
    0x01f6_0000,
    0x01f8_0000,
    0x01fa_0000,
    0x01fc_0000,
    0x01fe_0000,
    0x01e0_0000,
];
const CREDIT_CONTROL: u64 = 0x10;
const MEMORY_TYPE_CONTROL: u64 = 0x40;

#[derive(Default)]
pub struct MssNvlink {
    memory_type: [u32; BASES.len()],
}

impl MssNvlink {
    fn register(addr: u64) -> Option<(usize, u64)> {
        let register = addr & 0xffff;
        let base = addr - register;
        let link = BASES.iter().position(|&link| link == base)?;
        Some((link, register))
    }

    pub fn read(&self, addr: u64) -> Option<u32> {
        let (link, register) = Self::register(addr)?;
        match register {
            CREDIT_CONTROL => Some(0),
            MEMORY_TYPE_CONTROL => Some(self.memory_type[link]),
            _ => None,
        }
    }

    pub fn write(&mut self, addr: u64, value: u32) -> bool {
        let Some((link, register)) = Self::register(addr) else {
            return false;
        };
        match register {
            CREDIT_CONTROL => true, // The credit initialization trigger completes immediately.
            MEMORY_TYPE_CONTROL => {
                self.memory_type[link] = value & 3;
                true
            }
            _ => false,
        }
    }
}
