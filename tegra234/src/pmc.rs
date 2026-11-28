//! The PMC's scratch registers (`pmc@c360000`, its "scratch" window at 0x0c390000).
//!
//! They are plain retention registers: boot firmware (MB1/MB2, UEFI) and Linux keep state
//! there across resets. NVIDIA's `nvbootctrl` (L4T R36) maps them through `/dev/mem` for the
//! boot-chain bookkeeping: SCRATCH 0x3cc holds the bootloader slots' status bits (bit 0 slot A
//! unbootable, bit 1 slot B), 0x3a4 and 0x3a8 the rootfs A/B state (0x3a8: magic 0xface in
//! bits 0–15, which `nvbootctrl verify` clears to mark the boot good). At cold boot (all zero
//! here) both bootloader slots are good and the rootfs state is unset, as on a board whose
//! firmware boots chain A without rootfs A/B. The rest of the PMC is not modelled.
pub const SCRATCH: u64 = 0x0c39_0000;
pub const SCRATCH_SIZE: u64 = 0x1_0000;

pub struct Scratch {
    words: Vec<u32>,
}

impl Default for Scratch {
    fn default() -> Self {
        Self {
            words: vec![0; (SCRATCH_SIZE / 4) as usize],
        }
    }
}

impl Scratch {
    pub fn contains(addr: u64) -> bool {
        (SCRATCH..SCRATCH + SCRATCH_SIZE).contains(&addr)
    }

    pub fn read(&self, addr: u64) -> u32 {
        self.words[((addr - SCRATCH) / 4) as usize]
    }

    pub fn write(&mut self, addr: u64, value: u32) {
        self.words[((addr - SCRATCH) / 4) as usize] = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_registers_hold_what_is_written() {
        let mut s = Scratch::default();
        assert!(Scratch::contains(SCRATCH + 0x3cc) && !Scratch::contains(SCRATCH + SCRATCH_SIZE));
        assert_eq!(s.read(SCRATCH + 0x3cc), 0);
        s.write(SCRATCH + 0x3a8, 0xface);
        assert_eq!(s.read(SCRATCH + 0x3a8), 0xface);
    }
}
