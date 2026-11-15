//! SD card image backed by a file, copy-on-write: the image is opened read-only and every
//! written sector lives in an in-memory overlay (optionally saved/loaded as an overlay file);
//! with `--sd-persist` writes go to the file itself. The image is locked: shared while it is
//! only read, exclusive with `--sd-persist`, so a guest never writes a filesystem that another
//! emulator is running from (two writers corrupt it; a reader sees it change underneath).
use emu_core::disk::{Disk, Overlay};
use std::fs::File;
use std::os::unix::fs::FileExt;

pub struct CowFile { file: File, sectors: u64, overlay: Overlay, persist: bool }

impl CowFile {
    pub fn open(path: &str) -> std::io::Result<Self> {
        let file = File::open(path)?;
        file.try_lock_shared().map_err(|_| in_use(path))?;
        let sectors = file.metadata()?.len() / 512;
        Ok(CowFile { file, sectors, overlay: Overlay::default(), persist: false })
    }
    /// Writes go straight to the image file (`--sd-persist`): what the guest installs survives.
    pub fn open_persistent(path: &str) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
        file.try_lock().map_err(|_| in_use(path))?;
        let sectors = file.metadata()?.len() / 512;
        Ok(CowFile { file, sectors, overlay: Overlay::default(), persist: true })
    }
}

/// A disk shared by successive machines of one run, so the overlay survives a guest reboot
/// (the serial runner builds a new machine on reset).
#[derive(Clone)]
pub struct SharedDisk(pub std::rc::Rc<std::cell::RefCell<CowFile>>);

impl SharedDisk {
    /// Open `--sd PATH` (copy-on-write) or, with `--sd-persist`, write-through.
    /// The card is an SDHC card: 512 KiB to 2 TiB (its CSD counts 512 KiB units). A size
    /// that is not a multiple of 512 KiB works; the guest does not see the partial tail.
    pub fn open(path: &str, persist: bool) -> std::io::Result<Self> {
        let disk = if persist { CowFile::open_persistent(path)? } else { CowFile::open(path)? };
        let sectors = disk.sectors();
        if !(1024..=(1u64 << 32)).contains(&sectors) {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput,
                "an SD (SDHC) image must be between 512 KiB and 2 TiB"));
        }
        if sectors % 1024 != 0 {
            eprintln!("[sd] {path}: not a multiple of 512 KiB; the last {} KiB are not visible to the guest", sectors % 1024 / 2);
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
        let valid = (self.sectors.saturating_sub(lba).saturating_mul(512)).min(out.len() as u64) as usize;
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
            if valid > 0 { let _ = self.file.write_all_at(&data[..valid], lba * 512); }
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
}
