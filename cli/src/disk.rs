//! SD card image backed by a file, copy-on-write: the image is opened read-only and every
//! written sector lives in an in-memory overlay (optionally saved/loaded as an overlay file);
//! with `--sd-persist` writes go to the file itself. The image is locked: shared while it is
//! only read, exclusive with `--sd-persist`, so a guest never writes a filesystem that another
//! emulator is running from (two writers corrupt it; a reader sees it change underneath).
use emu_core::disk::{Disk, Overlay};
use std::fs::File;
use std::os::unix::fs::FileExt;

/// `sectors` is what the guest sees; the file holds the first `stored` of them (a partial last
/// sector counts, the rest of it zeros).
pub struct CowFile { file: File, path: String, stored: u64, sectors: u64, overlay: Overlay, persist: bool, reported: bool }

impl CowFile {
    pub fn open(path: &str) -> std::io::Result<Self> {
        let file = File::open(path)?;
        file.try_lock_shared().map_err(|_| in_use(path))?;
        let sectors = file.metadata()?.len().div_ceil(512);
        Ok(CowFile { file, path: path.into(), stored: sectors, sectors, overlay: Overlay::default(), persist: false, reported: false })
    }
    /// Writes go straight to the image file (`--sd-persist`): what the guest installs survives.
    pub fn open_persistent(path: &str) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
        file.try_lock().map_err(|_| in_use(path))?;
        let sectors = file.metadata()?.len().div_ceil(512);
        Ok(CowFile { file, path: path.into(), stored: sectors, sectors, overlay: Overlay::default(), persist: true, reported: false })
    }
    /// The image as an SD card: as large as a card's 512 KiB units need it to be, so that a card
    /// never ends inside an image's last partition. The added sectors read as zeros until they
    /// are written, and are kept like the others (with `--sd-persist` the file grows). An SDHC card
    /// is 512 KiB to 2 TiB: a smaller file is refused, not turned into a card.
    pub fn as_card(mut self) -> std::io::Result<Self> {
        self.check_card_size()?;
        self.sectors = self.stored.next_multiple_of(emu_core::sdcard::UNIT);
        Ok(self)
    }
    /// The disk interface has no errors to give the guest; say it once.
    fn report(&mut self, what: &str, e: std::io::Error) {
        if !std::mem::replace(&mut self.reported, true) {
            eprintln!("[sd] {}: {what}: {e}", self.path);
        }
    }
    /// The file grows to the whole disk at the first write past its end, so that a partition the
    /// guest grew is all there (the added part is sparse).
    fn grow(&mut self) -> std::io::Result<()> {
        self.file.set_len(self.sectors * 512)?;
        self.stored = self.sectors;
        Ok(())
    }
    fn check_card_size(&self) -> std::io::Result<()> {
        if (emu_core::sdcard::UNIT * 512..=2u64 << 40).contains(&self.file.metadata()?.len()) {
            return Ok(());
        }
        Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "an SD (SDHC) or eMMC image must be between 512 KiB and 2 TiB"))
    }
}

/// A disk shared by successive machines of one run, so the overlay survives a guest reboot
/// (the serial runner builds a new machine on reset).
#[derive(Clone)]
pub struct SharedDisk(pub std::rc::Rc<std::cell::RefCell<CowFile>>);

impl SharedDisk {
    /// Open `--sd PATH` (copy-on-write) or, with `--sd-persist`, write-through.
    pub fn open(path: &str, persist: bool) -> std::io::Result<Self> {
        Self::shared(Self::file(path, persist)?.as_card()?)
    }

    /// An eMMC image, at its exact size: an eMMC above 2 GiB reports its sector count. A smaller
    /// one counts in units like a card.
    pub fn open_emmc(path: &str, persist: bool) -> std::io::Result<Self> {
        let disk = Self::file(path, persist)?;
        if disk.stored <= 2 << 21 {
            return Self::shared(disk.as_card()?);
        }
        disk.check_card_size()?;
        Self::shared(disk)
    }

    fn file(path: &str, persist: bool) -> std::io::Result<CowFile> {
        if persist { CowFile::open_persistent(path) } else { CowFile::open(path) }
    }

    fn shared(disk: CowFile) -> std::io::Result<Self> {
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
        // (the file may end inside its last sector)
        let mut done = 0;
        while done < valid {
            match self.file.read_at(&mut out[done..valid], lba * 512 + done as u64) {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    self.report("reading the image failed, the guest gets zeros", e);
                    break;
                }
            }
        }
        out[done..valid].fill(0);
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
            if valid > 0 {
                let past = lba * 512 + valid as u64 > self.stored * 512;
                let done = if past { self.grow() } else { Ok(()) };
                if let Err(e) = done.and_then(|()| self.file.write_all_at(&data[..valid], lba * 512)) {
                    self.report("writing the image failed, the guest's data is lost", e);
                }
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
            let open = || if persist { CowFile::open_persistent(p) } else { CowFile::open(p) }.unwrap().as_card().unwrap();
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

    #[test]
    fn small_files_are_refused_and_a_partial_last_sector_is_stored() {
        let path = std::env::temp_dir().join(format!("sbc-emu-small-{}.img", std::process::id()));
        let p = path.to_str().unwrap();
        for n in [0, 2048, 1023 * 512, 1023 * 512 + 1, 1024 * 512 - 1] {
            std::fs::write(&path, vec![1u8; n]).unwrap();
            assert!(SharedDisk::open(p, true).is_err() && SharedDisk::open_emmc(p, true).is_err(), "{n} bytes");
            assert_eq!(std::fs::metadata(&path).unwrap().len(), n as u64);
        }
        // 1500 sectors and 100 bytes
        std::fs::write(&path, vec![7u8; 1500 * 512 + 100]).unwrap();
        assert_eq!(SharedDisk::open_emmc(p, false).unwrap().sectors(), 2048);
        // above 2 GiB: exact (sparse)
        let f = std::fs::File::create(format!("{p}.big")).unwrap();
        f.set_len((2 << 30) + 300 * 512).unwrap();
        drop(f);
        assert_eq!(SharedDisk::open_emmc(&format!("{p}.big"), false).unwrap().sectors(), (4 << 20) + 300);
        std::fs::remove_file(format!("{p}.big")).unwrap();
        for persist in [false, true] {
            let mut d = SharedDisk::open(p, persist).unwrap();
            assert_eq!(d.sectors(), 2048);
            let mut b = [1u8; 1024];
            for _ in 0..2 {
                d.read(1500, &mut b);
                assert!(b[..100] == [7; 100] && b[100..] == [0; 924], "persist {persist}");
                d.write(1502, &[2; 512]); // past it: what was never written stays the same
            }
        }
        std::fs::remove_file(&path).unwrap();
    }
}
