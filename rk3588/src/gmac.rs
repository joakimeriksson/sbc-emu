//! Initial DesignWare GMAC4 register and descriptor model for RK3588 GMAC1.
//! DMA addresses are guest physical addresses and are always bounds checked.
use std::collections::{BTreeMap, VecDeque};
use crate::phy::Yt8531;

const OWN: u32 = 1 << 31;
const FIRST: u32 = 1 << 29;
const LAST: u32 = 1 << 28;
const TI: u32 = 1;
const RI: u32 = 1 << 6;
const NIS: u32 = 1 << 15;
const FBE: u32 = 1 << 12;
const AIS: u32 = 1 << 14;

#[derive(Default)]
pub struct Gmac {
    regs: BTreeMap<u64, u32>,
    pub phy: Yt8531,
    tx_index: u32,
    rx_index: u32,
    tx_partial: Vec<u8>,
    tx_dropping: bool,
    tx_active: bool,
    pub tx_out: Vec<Vec<u8>>,
    pub rx_queue: VecDeque<Vec<u8>>,
}
impl Gmac {
    pub fn read(&self, off: u64) -> u32 {
        match off {
            0x1160 => {
                let status=self.read_raw(off) & !(NIS|AIS);
                status | if status & self.read_raw(0x1134) & (TI|RI)!=0 {NIS}else{0}
                    | if status & self.read_raw(0x1134) & FBE!=0 {AIS}else{0}
            }
            0x110 => 0x42, // GMAC 4.20
            0x1008 => u32::from(self.irq()), // channel interrupt summary
            _ => *self.regs.get(&off).unwrap_or(&0),
        }
    }
    pub fn write(&mut self, off: u64, mut value: u32) {
        match off {
            0x1000 if value & 1 != 0 => { // DMA software reset, PHY is external
                self.regs.clear(); self.tx_index = 0; self.rx_index = 0; self.tx_partial.clear(); self.tx_dropping=false; self.tx_active=false; self.rx_queue.clear(); self.tx_out.clear();
                value &= !1;
            }
            0x200 if value & 1 != 0 => {
                let addr = ((value >> 21) & 31) as u8;
                let reg = ((value >> 16) & 31) as u8;
                match (value >> 2) & 3 {
                    1 => self.phy.write(addr, reg, self.read(0x204) as u16),
                    3 => { let data = self.phy.read(addr, reg); self.regs.insert(0x204, data as u32); }
                    _ => {}
                }
                value &= !1;
            }
            0x1108 if value & 1 == 0 => {self.rx_queue.clear();}
            0x1160 => { let old = self.read_raw(off); value = old & !value; }
            0x1114 => self.tx_index = 0,
            0x111c => self.rx_index = 0,
            _ => {}
        }
        self.regs.insert(off, value);
    }
    pub fn irq(&self) -> bool {
        let status = self.read_raw(0x1160);
        let enabled = self.read_raw(0x1134);
        (enabled & NIS != 0 && status & enabled & (TI | RI) != 0) ||
        (enabled & AIS != 0 && status & enabled & FBE != 0)
    }
    fn read_raw(&self, off: u64) -> u32 { *self.regs.get(&off).unwrap_or(&0) }
    fn signal(&mut self, flag: u32) { let v = self.read_raw(0x1160) | flag; self.regs.insert(0x1160, v); }
    fn descriptor_addr(&self, base: u64, index: u32) -> Option<usize> {
        let lo = self.read_raw(base) as u64;
        let hi = self.read_raw(base - 4) as u64;
        usize::try_from((hi << 32 | lo).checked_add(index as u64 * 16)?).ok()
    }
    fn word(ram: &[u8], addr: usize) -> Option<u32> {
        Some(u32::from_le_bytes(ram.get(addr..addr.checked_add(4)?)?.try_into().ok()?))
    }
    fn put_word(ram: &mut [u8], addr: usize, value: u32) -> bool {
        let Some(end) = addr.checked_add(4) else { return false };
        let Some(slice) = ram.get_mut(addr..end) else { return false };
        slice.copy_from_slice(&value.to_le_bytes()); true
    }
    pub fn receiving(&self)->bool {self.read_raw(0x1108)&1!=0}
    fn bus_error(&mut self) {self.signal(FBE); self.regs.insert(0x1104,self.read_raw(0x1104)&!1);self.regs.insert(0x1108,self.read_raw(0x1108)&!1);self.rx_queue.clear();}
    /// Service owned TX descriptors and queued received frames.
    pub fn service(&mut self, ram: &mut [u8]) {
        if self.read_raw(0x1104) & 1 != 0 {
            let count = (self.read_raw(0x112c) & 0x3ff) + 1;
            for _ in 0..count {
                let Some(d) = self.descriptor_addr(0x1114, self.tx_index) else { self.bus_error(); break };
                let Some(flags) = Self::word(ram, d.saturating_add(12)) else { self.bus_error(); break };
                if flags & OWN == 0 { break }
                if flags & (1<<30) != 0 {
                    // Context descriptors have no data buffer; unsupported offloads are not advertised.
                    Self::put_word(ram,d+12,1<<30);
                    self.tx_index=(self.tx_index+1)%count;
                    continue;
                }
                let Some(ptr) = Self::word(ram, d) else { self.bus_error(); break };
                let Some(len) = Self::word(ram, d.saturating_add(8)).map(|v| (v & 0x3fff) as usize) else { self.bus_error(); break };
                let Some(data) = ram.get(ptr as usize..(ptr as usize).saturating_add(len)) else { self.bus_error(); break };
                if flags & FIRST != 0 { self.tx_partial.clear();self.tx_dropping=false;self.tx_active=true; }
                else if !self.tx_active {self.tx_dropping=true;}
                if self.tx_partial.len().saturating_add(len) > 65536 { self.tx_partial.clear();self.tx_dropping=true; }
                if !self.tx_dropping {self.tx_partial.extend_from_slice(data);}
                if flags & LAST != 0 && !self.tx_dropping { self.tx_out.push(std::mem::take(&mut self.tx_partial)); }
                if flags & LAST != 0 {self.tx_active=false;}
                Self::put_word(ram, d + 12, flags & (FIRST|LAST));
                self.tx_index = (self.tx_index + 1) % count;
                self.signal(TI);
            }
        }
        if self.read_raw(0x1108) & 1 != 0 {
            let count = (self.read_raw(0x1130) & 0x3ff) + 1;
            for _ in 0..count {
                let Some(frame) = self.rx_queue.front() else { break };
                let Some(d) = self.descriptor_addr(0x111c, self.rx_index) else { self.bus_error(); break };
                let Some(flags) = Self::word(ram, d.saturating_add(12)) else { self.bus_error(); break };
                if flags & OWN == 0 { break }
                let Some(ptr) = Self::word(ram, d) else { self.bus_error(); break };
                let capacity = ((self.read_raw(0x1108) >> 1) & 0x3fff) as usize;
                if frame.len() + 4 > capacity || frame.len() + 4 > 0x7fff { self.rx_queue.pop_front(); continue }
                let start = ptr as usize;
                let Some(out) = ram.get_mut(start..start.saturating_add(frame.len() + 4)) else { self.bus_error(); break };
                out[..frame.len()].copy_from_slice(frame); out[frame.len()..].fill(0);
                let len = frame.len() as u32 + 4;
                Self::put_word(ram, d + 12, FIRST | LAST | len);
                self.rx_queue.pop_front();
                self.rx_index = (self.rx_index + 1) % count;
                self.signal(RI);
            }
        }
    }
}

emu_core::snap_struct!(Gmac { regs, phy, tx_index, rx_index, tx_partial, tx_dropping, tx_active, tx_out, rx_queue });

#[cfg(test)]
mod tests {
    use super::*;
    fn put(r: &mut [u8], a: usize, v: u32) { assert!(Gmac::put_word(r, a, v)); }
    #[test]
    fn mdio_transactions_and_interrupt_acknowledgement() {
        let mut g = Gmac::default();
        g.write(0x200, (1 << 21) | (2 << 16) | (3 << 2) | 1);
        assert_eq!(g.read(0x200) & 1, 0);
        assert_eq!(g.read(0x204), 0x4f51);
        g.write(0x1134, NIS | RI); g.signal(RI); assert!(g.irq());
        g.write(0x1160, RI | NIS); assert!(!g.irq());
    }
    #[test]
    fn dma_transmits_and_receives_with_bounded_guest_memory() {
        let mut g = Gmac::default(); let mut ram = vec![0u8; 4096];
        ram[512..516].copy_from_slice(b"test");
        put(&mut ram, 64, 512); put(&mut ram, 72, 4); put(&mut ram, 76, OWN | FIRST | LAST);
        g.write(0x1114, 64); g.write(0x1104, 1); g.service(&mut ram);
        assert_eq!(g.tx_out, vec![b"test".to_vec()]); assert_eq!(Gmac::word(&ram, 76).unwrap() & OWN, 0);
        put(&mut ram, 96, 1024); put(&mut ram, 108, OWN);
        g.write(0x111c, 96); g.write(0x1108, (128 << 1) | 1);
        g.rx_queue.push_back(b"reply".to_vec()); g.service(&mut ram);
        assert_eq!(&ram[1024..1029], b"reply"); assert_eq!(Gmac::word(&ram, 108), Some(FIRST | LAST | 9));
        put(&mut ram, 96, u32::MAX); put(&mut ram, 108, OWN);
        g.rx_queue.push_back(b"bad address".to_vec()); g.service(&mut ram);
        assert!(g.rx_queue.is_empty());
        assert_ne!(g.read(0x1160)&FBE,0);
    }
    #[test]
    fn masked_receive_is_reasserted_after_transmit_ack() {
        let mut g=Gmac::default();g.signal(RI|TI);g.write(0x1134,NIS|TI);
        g.write(0x1160,TI|NIS);assert!(!g.irq());
        g.write(0x1134,NIS|RI);assert!(g.irq());assert_ne!(g.read(0x1160)&NIS,0);
    }
    #[test]
    fn reset_discards_frames_and_context_does_not_dma() {
        let mut g=Gmac::default();g.rx_queue.push_back(vec![1]);g.tx_out.push(vec![2]);
        g.write(0x1000,1);assert!(g.rx_queue.is_empty()&&g.tx_out.is_empty());
        let mut ram=vec![0;128];put(&mut ram,64,u32::MAX);put(&mut ram,76,OWN|(1<<30));
        g.write(0x1114,64);g.write(0x1104,1);g.service(&mut ram);
        assert_eq!(Gmac::word(&ram,76),Some(1<<30));assert_eq!(g.read(0x1160)&FBE,0);
    }
    #[test]
    fn oversized_chain_is_consumed_without_emitting_a_suffix() {
        let mut g=Gmac::default();let mut ram=vec![0;100000];
        for i in 0..6 {
            put(&mut ram,64+i*16,1024);put(&mut ram,72+i*16,14000);
            put(&mut ram,76+i*16,OWN|if i==0 {FIRST}else{0}|if i==5 {LAST}else{0});
        }
        g.write(0x1114,64);g.write(0x112c,5);g.write(0x1104,1);g.service(&mut ram);
        assert!(g.tx_out.is_empty());for i in 0..6 {assert_eq!(Gmac::word(&ram,76+i*16).unwrap()&OWN,0);}
    }

}
