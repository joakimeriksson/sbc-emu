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

/// A restore that fails half way leaves the disks as they were, the card and the stick: they outlive
/// the machine. Nor does it take the JIT away.
#[test]
fn the_disks_survive_a_failed_restore() {
    let card = || Card { image: MemDisk::new(4096), written: Overlay::default(), persist: false, through: false };
    let new = |sector: u64| {
        let mut m = machine(false);
        m.soc.devs.storage = Some(bcm2711::usbdev::Storage::new(Box::new(card())));
        m.soc.sd.disk.as_mut().unwrap().write(sector, &[1; 512]);
        m.soc.devs.storage.as_mut().unwrap().disk.write(sector, &[1; 512]);
        m
    };
    let mut other = new(7);
    other.soc.ram[65000] = 1;
    let whole = miniz_oxide::inflate::decompress_to_vec(&other.save()[8..]).unwrap();
    let mut m = new(9);
    let jit = m.enable_jit();
    assert!(m.restore_raw(&whole[..whole.len() - 10]).unwrap_err().contains("truncated") && m.in_pieces);
    let (mut sd, mut stick) = ([0; 3 * 512], [0; 3 * 512]);
    m.soc.sd.disk.as_mut().unwrap().read(7, &mut sd);
    m.soc.devs.storage.as_mut().unwrap().disk.read(7, &mut stick);
    for s in [sd, stick] { assert_eq!([s[0], s[512], s[1024]], [0, 0, 1]); }
    assert_eq!(m.jit().is_some(), jit);
    // a page that is not there: its number is in front of its 4096 bytes
    let mut page = whole.clone();
    let at = whole.len() - 4100;
    assert_eq!(page[at..at + 4], (65000u32 / 4096).to_le_bytes());
    page[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(m.restore_raw(&page).unwrap_err().contains("out of range"));
    // and the whole one is as good as ever
    m.restore_raw(&whole).unwrap();
    m.soc.sd.disk.as_mut().unwrap().read(7, &mut sd);
    assert_eq!([sd[0], sd[512], sd[1024]], [1, 0, 0]);
    assert_eq!(m.jit().is_some(), jit);
}

/// What is set on the cores from outside (`--cpu-crypto`, a debugger) is not the guest's: it stays.
#[test]
fn a_restore_keeps_what_is_set_from_outside() {
    let mut m = Machine::with_cpus(65 << 20, 2);
    let snap = m.save();
    m.set_crypto(true);
    m.set_debugging(true);
    for c in m.cpus.iter_mut() { c.watch = vec![(0x1000, 4, 1)]; }
    m.restore(&snap).unwrap();
    for c in &m.cpus { assert!(c.crypto && c.jit_off && c.watch == [(0x1000, 4, 1)]); }
}
