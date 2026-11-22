//! What a debugger sees of guest memory: through a hart's page tables, without the side effects of
//! the guest's own accesses (no faults latched, no A or D bits, no device registers).
use crate::bus::{LP_BASE, LP_SIZE, SRAM_BASE, SRAM_SIZE};
use crate::soc::Machine;
use emu_core::Bus as _;

fn byte(m: &mut Machine, pa: u32) -> Option<u8> { m.bus.fetch_ahead(pa).map(|b| b[0]) }

fn translate(m: &mut Machine, core: usize, va: u32) -> Option<u32> {
    let bus = &mut m.bus;
    m.cores[core].debug_translate(va, |pa| {
        let mut w = [0; 4];
        for (k, b) in w.iter_mut().enumerate() { *b = bus.fetch_ahead(pa.wrapping_add(k as u32))?[0]; }
        Some(u32::from_le_bytes(w))
    })
}

/// Memory (RAM, ROM, flash, PSRAM) at virtual address `va` as `core` sees it.
pub fn read(m: &mut Machine, core: usize, va: u32, len: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(len);
    let (mut page, mut base) = (None, 0);
    for k in 0..len as u32 {
        let a = va.wrapping_add(k);
        if page != Some(a >> 12) { base = translate(m, core, a & !0xfff)?; page = Some(a >> 12); }
        out.push(byte(m, base | (a & 0xfff))?);
    }
    Some(out)
}

/// Write RAM (SRAM, LP RAM, mapped PSRAM) at virtual address `va`.
pub fn write(m: &mut Machine, core: usize, va: u32, data: &[u8]) -> bool {
    let mut to = Vec::with_capacity(data.len());
    for k in 0..data.len() as u32 {
        let Some(pa) = translate(m, core, va.wrapping_add(k)) else { return false };
        let ram = pa.wrapping_sub(SRAM_BASE) < SRAM_SIZE as u32 || pa.wrapping_sub(LP_BASE) < LP_SIZE as u32 || (crate::cache::PSRAM_VADDR..0x5400_0000).contains(&pa);
        if !ram || byte(m, pa).is_none() { return false; }
        to.push(pa);
    }
    to.iter().zip(data).all(|(&pa, &b)| m.bus.write8(pa, b).is_ok())
}
