//! A snapshot with written sectors is not restored onto an image that is written through
//! (`--sd-persist`): the guest would go on with caches of sectors the image does not have.
use bcm2711::Machine;
use emu_core::disk::{Disk, MemDisk, Overlay};

struct Card { image: MemDisk, written: Overlay, persist: bool, through: bool }
impl Disk for Card {
    fn sectors(&self) -> u64 { self.image.sectors() }
    fn read(&mut self, lba: u64, out: &mut [u8]) { self.image.read(lba, out); self.written.apply(lba, out); }
    fn write(&mut self, lba: u64, data: &[u8]) {
        let n = self.sectors();
        if self.persist { self.through = true; self.image.write(lba, data) } else { self.written.store(lba, data, n) }
    }
    fn overlay(&self) -> Vec<(u64, Vec<u8>)> { self.written.sorted() }
    fn restore_overlay(&mut self, sectors: Vec<(u64, Vec<u8>)>) { let n = self.sectors(); if !self.persist { self.written.replace(sectors, n) } }
    fn snapshots_disk(&self) -> bool { !self.persist }
    fn written_through(&self) -> bool { self.through }
}

fn machine(persist: bool) -> Machine {
    let mut m = Machine::new(65 << 20);
    m.soc.sd.disk = Some(Box::new(Card { image: MemDisk::new(4096), written: Overlay::default(), persist, through: false }));
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

/// Going back in a session that has written to the image is refused, and before anything is
/// changed; a failure half way is known as one.
#[test]
fn a_refused_restore_changes_nothing() {
    let mut m = machine(true);
    let early = m.save();
    m.cpus[0].x[5] = 77;
    m.soc.ram[0x1000] = 9;
    m.restore(&early).unwrap(); // nothing written yet
    assert_eq!((m.cpus[0].x[5], m.soc.ram[0x1000]), (0, 0));
    m.cpus[0].x[5] = 77;
    m.soc.ram[0x1000] = 9;
    m.soc.sd.disk.as_mut().unwrap().write(7, &[1; 512]);
    assert!(m.restore(&early).unwrap_err().contains("written to the image"));
    assert!(m.cpus[0].x[5] == 77 && m.soc.ram[0x1000] == 9 && !m.in_pieces);
    // another machine's: refused at once as well
    assert!(Machine::new(66 << 20).restore(&early).unwrap_err().contains("RAM"));
    // one with written sectors is found out half way
    let mut cow = machine(false);
    cow.soc.sd.disk.as_mut().unwrap().write(7, &[1; 512]);
    let mut m = machine(true);
    assert!(m.restore(&cow.save()).is_err() && m.in_pieces);
    // and a good one makes it whole
    m.restore(&early).unwrap();
    assert!(!m.in_pieces);
}

/// A restore that fails half way leaves the disks as they were: they outlive the machine.
#[test]
fn the_card_survives_a_failed_restore() {
    let mut other = machine(false);
    other.soc.sd.disk.as_mut().unwrap().write(7, &[1; 512]);
    other.soc.ram[65000] = 1;
    let mut raw = miniz_oxide::inflate::decompress_to_vec(&other.save()[8..]).unwrap();
    raw.truncate(raw.len() - 10);
    let mut m = machine(false);
    m.soc.sd.disk.as_mut().unwrap().write(9, &[2; 512]);
    assert!(m.restore_raw(&raw).is_err() && m.in_pieces);
    let mut s = [0; 1024 * 2];
    m.soc.sd.disk.as_mut().unwrap().read(7, &mut s);
    assert!(s[..512].iter().all(|&b| b == 0) && s[1024..1536].iter().all(|&b| b == 2));
}
