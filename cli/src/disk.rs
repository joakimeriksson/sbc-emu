//! SD card image backed by a file, copy-on-write: the image is opened read-only and every
//! written sector lives in an in-memory overlay (optionally saved/loaded as an overlay file);
//! with `--sd-persist` writes go to the file itself. The image is locked: shared while it is
//! only read, exclusive with `--sd-persist`, so a guest never writes a filesystem that another
//! emulator is running from (two writers corrupt it; a reader sees it change underneath).
use emu_core::disk::{Disk, Overlay};
use std::fs::File;
use std::os::unix::fs::FileExt;

/// `sectors` is what the guest sees; the file holds the first `stored` of them.
pub struct CowFile { file: File, stored: u64, sectors: u64, overlay: Overlay, persist: bool }

impl CowFile {
    pub fn open(path: &str) -> std::io::Result<Self> {
        let file = File::open(path)?;
        file.try_lock_shared().map_err(|_| in_use(path))?;
        let sectors = file.metadata()?.len() / 512;
        Ok(CowFile { file, stored: sectors, sectors, overlay: Overlay::default(), persist: false })
    }
    /// Writes go straight to the image file (`--sd-persist`): what the guest installs survives.
    pub fn open_persistent(path: &str) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
        file.try_lock().map_err(|_| in_use(path))?;
        let sectors = file.metadata()?.len() / 512;
        Ok(CowFile { file, stored: sectors, sectors, overlay: Overlay::default(), persist: true })
    }
    /// The image as an SD card: as large as a card's 512 KiB units need it to be, so that a card
    /// never ends inside an image's last partition. The added sectors read as zeros until they
    /// are written, and are kept like the others (with `--sd-persist` the file grows).
    pub fn as_card(mut self) -> Self {
        self.sectors = self.stored.next_multiple_of(emu_core::sdcard::UNIT);
        self
    }
}

/// A disk shared by successive machines of one run, so the overlay survives a guest reboot
/// (the serial runner builds a new machine on reset).
#[derive(Clone)]
pub struct SharedDisk(pub std::rc::Rc<std::cell::RefCell<CowFile>>);

impl SharedDisk {
    /// Open `--sd PATH` (copy-on-write) or, with `--sd-persist`, write-through.
    /// The card is an SDHC card: 512 KiB to 2 TiB (its CSD counts 512 KiB units; see `as_card`).
    pub fn open(path: &str, persist: bool) -> std::io::Result<Self> {
        let disk = if persist { CowFile::open_persistent(path)? } else { CowFile::open(path)? }.as_card();
        let sectors = disk.sectors();
        if !(1024..=(1u64 << 32)).contains(&sectors) {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput,
                "an SD (SDHC) image must be between 512 KiB and 2 TiB"));
        }
        Ok(SharedDisk(std::rc::Rc::new(std::cell::RefCell::new(disk))))
    }
}

impl Disk for SharedDisk {
    fn sectors(&self) -> u64 { self.0.borrow().sectors() }
    fn read(&mut self, lba: u64, out: &mut [u8]) { self.0.borrow_mut().read(lba, out); }
    fn write(&mut self, lba: u64, data: &[u8]) { self.0.borrow_mut().write(lba, data); }
    fn overlay(&self) -> Vec<(u64, Vec<u8>)> { self.0.borrow().overlay() }
    fn restore_overlay(&mut self, sectors: Vec<(u64, Vec<u8>)>) { self.0.borrow_mut().restore_overlay(sectors); }
    fn snapshots_disk(&self) -> bool { self.0.borrow().snapshots_disk() }
}

fn in_use(_path: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::WouldBlock,
        "in use by another sbc-emu (--sd-persist needs the image to itself; copy it for a second run)")
}

impl Disk for CowFile {
    fn sectors(&self) -> u64 { self.sectors }
    fn read(&mut self, lba: u64, out: &mut [u8]) {
        // One host read for the valid part of the run (past the end reads as zero), then
        // patch overlaid sectors.
        let valid = (self.stored.saturating_sub(lba).saturating_mul(512)).min(out.len() as u64) as usize;
        out[valid..].fill(0);
        if valid > 0 && self.file.read_exact_at(&mut out[..valid], lba * 512).is_err() {
            out[..valid].fill(0);
        }
        self.overlay.apply(lba, out);
    }
    fn overlay(&self) -> Vec<(u64, Vec<u8>)> { self.overlay.sorted() }
    fn restore_overlay(&mut self, sectors: Vec<(u64, Vec<u8>)>) {
        if self.persist {
            // Never mix a snapshot's copy-on-write sectors into a persistent image.
            if !sectors.is_empty() {
                eprintln!("[sd] --sd-persist: the snapshot's {} SD sectors are not written into the image", sectors.len());
            }
        } else {
            self.overlay.replace(sectors, self.sectors);
        }
    }
    fn snapshots_disk(&self) -> bool { !self.persist }
    fn write(&mut self, lba: u64, data: &[u8]) {
        if self.persist {
            let valid = (self.sectors.saturating_sub(lba).saturating_mul(512)).min(data.len() as u64) as usize;
            if valid > 0 && self.file.write_all_at(&data[..valid], lba * 512).is_ok() {
                self.stored = self.stored.max(lba + valid as u64 / 512);
            }
            return;
        }
        self.overlay.store(lba, data, self.sectors);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restoring_a_snapshot_drops_sectors_written_after_it() {
        let path = std::env::temp_dir().join(format!("sbc-cow-{}.img", std::process::id()));
        std::fs::write(&path, vec![0xaa; 4 * 512]).unwrap();
        let mut disk = CowFile::open(path.to_str().unwrap()).unwrap();
        disk.write(1, &[1; 512]);
        let snapshot = disk.overlay();
        disk.write(2, &[2; 512]); // after the snapshot
        disk.restore_overlay(snapshot);
        let mut out = vec![0; 3 * 512];
        disk.read(1, &mut out);
        assert!(out[..512].iter().all(|&b| b == 1), "snapshot sector kept");
        assert!(out[512..1024].iter().all(|&b| b == 0xaa), "later write dropped");
        assert!(out[1024..].iter().all(|&b| b == 0xaa));
        // Past the end: the valid sector is read, the rest is zero.
        let mut tail = vec![0x55; 1024];
        disk.read(3, &mut tail);
        assert!(tail[..512].iter().all(|&b| b == 0xaa) && tail[512..].iter().all(|&b| b == 0));
        assert!(disk.snapshots_disk());
        drop(disk);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_card_keeps_what_is_written_past_an_odd_sized_image() {
        for persist in [false, true] {
            let path = std::env::temp_dir().join(format!("sbc-emu-odd-{}-{persist}.img", std::process::id()));
            let p = path.to_str().unwrap();
            std::fs::write(&path, vec![7u8; 1500 * 512]).unwrap();
            let open = || if persist { CowFile::open_persistent(p) } else { CowFile::open(p) }.unwrap().as_card();
            let mut d = open();
            assert_eq!(d.sectors(), 2048);
            let mut out = [1u8; 1024];
            d.read(1499, &mut out); // the last stored sector and the first added one
            assert_eq!((out[0], out[511], out[512], out[1023]), (7, 7, 0, 0));
            d.write(2047, &[9; 512]);
            d.write(1499, &[5; 1024]);
            d.write(2048, &[3; 512]); // past the card
            let check = |d: &mut CowFile| {
                let mut b = [1u8; 512];
                for (lba, want) in [(1498, 7), (1499, 5), (1500, 5), (1501, 0), (2046, 0), (2047, 9), (2048, 0)] {
                    d.read(lba, &mut b);
                    assert_eq!(b, [want; 512], "sector {lba}, persist {persist}");
                }
            };
            check(&mut d);
            drop(d);
            assert_eq!(std::fs::metadata(&path).unwrap().len(), if persist { 2048 } else { 1500 } * 512);
            if persist {
                check(&mut open());
            }
            std::fs::remove_file(&path).unwrap();
        }
    }
}
