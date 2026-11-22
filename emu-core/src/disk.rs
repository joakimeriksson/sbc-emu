//! Sector storage shared by the emulated storage controllers (SD/MMC, eMMC, USB mass
//! storage) of every board.
//!
//! Contract: 512-byte sectors; `read`/`write` take whole sectors. Sectors at or beyond
//! `sectors()` read as zero and ignore writes (the valid leading part of a request is
//! still transferred). Controllers report out-of-range requests to the guest themselves.

/// Storage behind a storage controller (512-byte sectors).
pub trait Disk {
    fn sectors(&self) -> u64;
    fn read(&mut self, lba: u64, out: &mut [u8]);
    fn write(&mut self, lba: u64, data: &[u8]);
    /// Sectors written since the image was opened (copy-on-write disks), sorted by LBA,
    /// for snapshots. Empty for disks that write through to their backing store.
    fn overlay(&self) -> Vec<(u64, Vec<u8>)> {
        Vec::new()
    }
    /// Make the written sectors exactly `sectors` (a snapshot's overlay): sectors written
    /// after the snapshot was taken must read as the image again. The default writes the
    /// snapshot's sectors over the disk, which is only correct for disks without an overlay.
    fn restore_overlay(&mut self, sectors: Vec<(u64, Vec<u8>)>) {
        for (lba, data) in sectors {
            self.write(lba, &data);
        }
    }
    /// Whether `overlay` captures every write (false for write-through disks such as
    /// `--sd-persist`, whose snapshots cannot restore the disk).
    fn snapshots_disk(&self) -> bool {
        true
    }
    /// Whether anything has been written through to the backing store since it was opened: an
    /// earlier snapshot then no longer fits it.
    fn written_through(&self) -> bool {
        false
    }
}

/// An in-memory disk (tests, small images).
pub struct MemDisk(pub Vec<u8>);

impl MemDisk {
    pub fn new(sectors: u64) -> Self {
        MemDisk(vec![0; sectors as usize * 512])
    }
}

impl Disk for MemDisk {
    fn sectors(&self) -> u64 {
        self.0.len() as u64 / 512
    }
    fn read(&mut self, lba: u64, out: &mut [u8]) {
        out.fill(0);
        let start = lba.saturating_mul(512).min(self.0.len() as u64) as usize;
        let n = out.len().min(self.0.len() - start);
        out[..n].copy_from_slice(&self.0[start..start + n]);
    }
    fn write(&mut self, lba: u64, data: &[u8]) {
        let start = lba.saturating_mul(512).min(self.0.len() as u64) as usize;
        let n = data.len().min(self.0.len() - start);
        self.0[start..start + n].copy_from_slice(&data[..n]);
    }
    /// There is no image behind it: all it holds.
    fn overlay(&self) -> Vec<(u64, Vec<u8>)> {
        let sectors = self.0.chunks(512).enumerate();
        sectors.filter(|(_, s)| s.iter().any(|&b| b != 0)).map(|(k, s)| (k as u64, s.to_vec())).collect()
    }
    fn restore_overlay(&mut self, sectors: Vec<(u64, Vec<u8>)>) {
        self.0.fill(0);
        for (lba, data) in sectors {
            self.write(lba, &data);
        }
    }
}

/// A copy-on-write overlay of written sectors, for disks whose image stays unchanged.
#[derive(Default)]
pub struct Overlay(std::collections::HashMap<u64, Box<[u8; 512]>>);

impl Overlay {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// Patch `out` (sectors from `lba`) with overlaid sectors.
    pub fn apply(&self, lba: u64, out: &mut [u8]) {
        if self.0.is_empty() {
            return;
        }
        for (k, chunk) in out.chunks_mut(512).enumerate() {
            if let Some(s) = self.0.get(&(lba + k as u64)) {
                chunk.copy_from_slice(&s[..chunk.len()]);
            }
        }
    }
    /// Record `data` written at `lba`, up to `limit` sectors.
    pub fn store(&mut self, lba: u64, data: &[u8], limit: u64) {
        for (k, chunk) in data.chunks(512).enumerate() {
            let sector = lba + k as u64;
            if sector >= limit {
                break;
            }
            let mut s = Box::new([0u8; 512]);
            s[..chunk.len()].copy_from_slice(chunk);
            self.0.insert(sector, s);
        }
    }
    pub fn sorted(&self) -> Vec<(u64, Vec<u8>)> {
        let mut v: Vec<(u64, Vec<u8>)> = self.0.iter().map(|(k, s)| (*k, s.to_vec())).collect();
        v.sort_by_key(|e| e.0);
        v
    }
    pub fn replace(&mut self, sectors: Vec<(u64, Vec<u8>)>, limit: u64) {
        self.0.clear();
        for (lba, data) in sectors {
            self.store(lba, &data, limit);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_disk_clips_out_of_range_requests() {
        let mut disk = MemDisk::new(2);
        disk.write(1, &[7; 1024]); // second sector past the end is ignored
        let mut out = [1u8; 1024];
        disk.read(1, &mut out);
        assert!(out[..512].iter().all(|&b| b == 7));
        assert!(out[512..].iter().all(|&b| b == 0));
    }

    #[test]
    fn overlay_restore_drops_later_writes() {
        let mut overlay = Overlay::default();
        overlay.store(3, &[1; 512], 10);
        let snapshot = overlay.sorted();
        overlay.store(4, &[2; 512], 10);
        overlay.replace(snapshot, 10);
        let mut out = [9u8; 1024];
        overlay.apply(3, &mut out);
        assert!(out[..512].iter().all(|&b| b == 1));
        assert!(out[512..].iter().all(|&b| b == 9), "sector 4 is the image again");
    }
}
