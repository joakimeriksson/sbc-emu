//! A snapshot with written sectors is not restored onto an image that is written through
//! (`--sd-persist`): the guest would go on with caches of sectors the image does not have.
use bcm2711::Machine;
use emu_core::disk::{Disk, MemDisk, Overlay};

struct Card { image: MemDisk, written: Overlay, persist: bool }
impl Disk for Card {
    fn sectors(&self) -> u64 { self.image.sectors() }
    fn read(&mut self, lba: u64, out: &mut [u8]) { self.image.read(lba, out); self.written.apply(lba, out); }
    fn write(&mut self, lba: u64, data: &[u8]) {
        let n = self.sectors();
        if self.persist { self.image.write(lba, data) } else { self.written.store(lba, data, n) }
    }
    fn overlay(&self) -> Vec<(u64, Vec<u8>)> { self.written.sorted() }
    fn restore_overlay(&mut self, sectors: Vec<(u64, Vec<u8>)>) { let n = self.sectors(); if !self.persist { self.written.replace(sectors, n) } }
    fn snapshots_disk(&self) -> bool { !self.persist }
}

fn machine(persist: bool) -> Machine {
    let mut m = Machine::new(65 << 20);
    m.soc.sd.disk = Some(Box::new(Card { image: MemDisk::new(4096), written: Overlay::default(), persist }));
    m
}

#[test]
fn written_sectors_and_a_persistent_image() {
    let mut m = machine(false);
    m.soc.sd.disk.as_mut().unwrap().write(7, &[1; 512]);
    let cow = m.save();
    machine(false).restore(&cow).unwrap();
    assert!(machine(true).restore(&cow).unwrap_err().contains("--sd-persist"));
    // one made with --sd-persist has none: accepted, for the image it was made with
    machine(true).restore(&machine(true).save()).unwrap();
}
