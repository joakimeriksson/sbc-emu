//! The SDIO card interface the DesignWare MMC host drives (as in sbc-emu's BCM2711 SDHCI):
//! function-0/1/2 register access (CMD52), block transfers (CMD53) and the card interrupt.

pub trait SdioCard {
    /// number of I/O functions besides function 0
    fn num_functions(&self) -> u8;
    /// CMD52: one register; returns the byte read (or the byte now in the register after a write)
    fn rw_direct(&mut self, write: bool, func: u8, addr: u32, data: u8, raw: bool) -> u8;
    /// CMD53 read/write of `out.len()` bytes (incrementing or fixed address)
    fn read_ext(&mut self, func: u8, addr: u32, incr: bool, out: &mut [u8]);
    fn write_ext(&mut self, func: u8, addr: u32, incr: bool, data: &[u8]);
    /// the card's interrupt line (DAT[1])
    fn irq(&self) -> bool;
    /// the card's out-of-band interrupt (a host-wake GPIO), for hosts that use one
    fn oob_irq(&self) -> bool {
        self.irq()
    }
    /// time passes (ns): firmware timers, delayed events
    fn tick(&mut self, _now_ns: u64) {}
    /// when the card next has something to do on its own (ns), for the machine's idle deadline
    fn next_event(&self) -> Option<u64> {
        None
    }
    /// network frames the card transmitted (Ethernet II)
    fn take_tx(&mut self) -> Vec<Vec<u8>> { Vec::new() }
    /// a network frame for the card
    fn deliver(&mut self, _eth: &[u8]) {}
    fn set_trace(&mut self, _on: bool) {}
    fn save_state(&self, _w: &mut emu_core::snap::W) {}
    fn load_state(&mut self, _r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> { Ok(()) }
}
