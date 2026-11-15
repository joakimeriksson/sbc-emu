//! SD card image backed by a file, copy-on-write: the image is opened read-only and every
//! written sector lives in an in-memory overlay (optionally saved/loaded as an overlay file);
//! with `--sd-persist` writes go to the file itself. The image is locked: shared while it is
//! only read, exclusive with `--sd-persist`, so a guest never writes a filesystem that another
//! emulator is running from (two writers corrupt it; a reader sees it change underneath).
use emu_core::disk::Disk;
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;

pub struct CowFile { file: File, sectors: u64, overlay: HashMap<u64, Box<[u8; 512]>>, persist: bool }

impl CowFile {
    pub fn open(path: &str) -> std::io::Result<Self> {
        let file = File::open(path)?;
        file.try_lock_shared().map_err(|_| in_use(path))?;
        let sectors = file.metadata()?.len() / 512;
        Ok(CowFile { file, sectors, overlay: HashMap::new(), persist: false })
    }
    /// Writes go straight to the image file (`--sd-persist`): what the guest installs survives.
    pub fn open_persistent(path: &str) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
        file.try_lock().map_err(|_| in_use(path))?;
        let sectors = file.metadata()?.len() / 512;
        Ok(CowFile { file, sectors, overlay: HashMap::new(), persist: true })
    }
}

fn in_use(_path: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::WouldBlock,
        "in use by another sbc-emu (--sd-persist needs the image to itself; copy it for a second run)")
}

impl Disk for CowFile {
    fn sectors(&self) -> u64 { self.sectors }
    fn read(&mut self, lba: u64, out: &mut [u8]) {
        let n = out.len() / 512;
        // one host read for the whole run, then patch overlaid sectors
        let _ = self.file.read_exact_at(out, lba * 512).or_else(|_| { out.fill(0); Ok::<(), ()>(()) });
        if !self.overlay.is_empty() {
            for k in 0..n {
                if let Some(s) = self.overlay.get(&(lba + k as u64)) { out[k * 512..(k + 1) * 512].copy_from_slice(&s[..]); }
            }
        }
    }
    fn overlay(&self) -> Vec<(u64, Vec<u8>)> {
        let mut v: Vec<(u64, Vec<u8>)> = self.overlay.iter().map(|(k, s)| (*k, s.to_vec())).collect();
        v.sort_by_key(|e| e.0);
        v
    }
    fn write(&mut self, lba: u64, data: &[u8]) {
        if self.persist {
            let _ = self.file.write_all_at(data, lba * 512);
            return;
        }
        for (k, chunk) in data.chunks(512).enumerate() {
            let mut s = Box::new([0u8; 512]);
            s[..chunk.len()].copy_from_slice(chunk);
            self.overlay.insert(lba + k as u64, s);
        }
    }
}
