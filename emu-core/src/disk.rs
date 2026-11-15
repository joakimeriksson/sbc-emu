//! Sector storage shared by emulated storage controllers.

/// Storage behind the card (512-byte sectors).
pub trait Disk {
    fn sectors(&self) -> u64;
    fn read(&mut self, lba: u64, out: &mut [u8]);
    fn write(&mut self, lba: u64, data: &[u8]);
    /// Sectors written since the image was opened (copy-on-write disks), for snapshots.
    fn overlay(&self) -> Vec<(u64, Vec<u8>)> { Vec::new() }
}
