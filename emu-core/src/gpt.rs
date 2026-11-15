//! Read GPT partition tables from a sector disk. The caller chooses a partition's type and contents.
use crate::disk::Disk;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    /// One-based GPT entry number, including any unused entries before this one.
    pub number: u32,
    /// GUIDs as the 16 bytes stored on disk (first three fields are little endian).
    pub type_guid: [u8; 16],
    pub unique_guid: [u8; 16],
    pub first_lba: u64,
    pub last_lba: u64,
}
fn u32le(b: &[u8], p: usize) -> u32 {
    u32::from_le_bytes(b[p..p + 4].try_into().unwrap())
}
fn u64le(b: &[u8], p: usize) -> u64 {
    u64::from_le_bytes(b[p..p + 8].try_into().unwrap())
}
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for b in data {
        crc ^= *b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

/// Read the primary GPT header and table, validating checksums, disk bounds, and entry bounds.
/// Returns an error for a malformed table; entries outside the usable range are skipped.
/// No guest writes or journal recovery are performed.
pub fn partitions(disk: &mut dyn Disk) -> Result<Vec<Partition>, String> {
    let sectors = disk.sectors();
    if sectors < 3 {
        return Err("disk too small for GPT".into());
    }
    let mut mbr = [0u8; 512];
    disk.read(0, &mut mbr);
    if mbr[510..512] != [0x55, 0xaa] || !(0..4).any(|n| mbr[446 + n * 16 + 4] == 0xee) {
        return Err("no protective GPT MBR".into());
    }
    let mut h = [0u8; 512];
    disk.read(1, &mut h);
    if &h[..8] != b"EFI PART" {
        return Err("invalid GPT signature".into());
    }
    if u32le(&h, 8) != 0x0001_0000 {
        return Err("unsupported GPT revision".into());
    }
    let len = u32le(&h, 12) as usize;
    if !(92..=512).contains(&len) {
        return Err("invalid GPT header size".into());
    }
    let checksum = u32le(&h, 16);
    let mut check = h;
    check[16..20].fill(0);
    if crc32(&check[..len]) != checksum {
        return Err("GPT header checksum mismatch".into());
    }
    if u64le(&h, 24) != 1 || u64le(&h, 32) >= sectors {
        return Err("invalid GPT header locations".into());
    }
    let first = u64le(&h, 40);
    let last = u64le(&h, 48);
    if first > last || last >= sectors {
        return Err("invalid GPT usable range".into());
    }
    let table_lba = u64le(&h, 72);
    let count = u32le(&h, 80);
    let size = u32le(&h, 84);
    // Bound allocations on untrusted images while accepting the standard 128 x 128 table.
    if count == 0 || count > 16384 || !(128..=4096).contains(&size) || size % 8 != 0 {
        return Err("unsupported GPT entry geometry".into());
    }
    let bytes = (count as u64)
        .checked_mul(size as u64)
        .ok_or("GPT table size overflow")?;
    if bytes > 16 * 1024 * 1024 {
        return Err("GPT table too large".into());
    }
    let blocks = bytes.div_ceil(512);
    if table_lba < 2
        || table_lba
            .checked_add(blocks)
            .is_none_or(|end| end > sectors)
    {
        return Err("GPT table outside disk".into());
    }
    let mut table = vec![0; blocks as usize * 512];
    disk.read(table_lba, &mut table);
    if crc32(&table[..bytes as usize]) != u32le(&h, 88) {
        return Err("GPT table checksum mismatch".into());
    }
    let mut entries = Vec::new();
    for n in 0..count as usize {
        let e = &table[n * size as usize..(n + 1) * size as usize];
        if e[..16] == [0; 16] {
            continue;
        }
        let start = u64le(e, 32);
        let end = u64le(e, 40);
        if start < first || start > end || end > last {
            continue;
        }
        entries.push(Partition {
            number: n as u32 + 1,
            type_guid: e[..16].try_into().unwrap(),
            unique_guid: e[16..32].try_into().unwrap(),
            first_lba: start,
            last_lba: end,
        });
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Mem(Vec<u8>);
    impl Disk for Mem {
        fn sectors(&self) -> u64 {
            self.0.len() as u64 / 512
        }
        fn read(&mut self, lba: u64, out: &mut [u8]) {
            let p = lba as usize * 512;
            out.copy_from_slice(&self.0[p..p + out.len()]);
        }
        fn write(&mut self, lba: u64, data: &[u8]) {
            let p = lba as usize * 512;
            self.0[p..p + data.len()].copy_from_slice(data);
        }
    }
    fn fixture() -> Mem {
        let mut d = Mem(vec![0; 64 * 512]);
        d.0[510..512].copy_from_slice(&[0x55, 0xaa]);
        d.0[450] = 0xee;
        let table = &mut d.0[1024..1536];
        table[128] = 1;
        table[144] = 2;
        table[160..168].copy_from_slice(&8u64.to_le_bytes());
        table[168..176].copy_from_slice(&20u64.to_le_bytes());
        let sum = crc32(table);
        let h = &mut d.0[512..1024];
        h[..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x10000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        for (o, v) in [(24, 1u64), (32, 63), (40, 3), (48, 62), (72, 2)] {
            h[o..o + 8].copy_from_slice(&v.to_le_bytes());
        }
        h[80..84].copy_from_slice(&4u32.to_le_bytes());
        h[84..88].copy_from_slice(&128u32.to_le_bytes());
        h[88..92].copy_from_slice(&sum.to_le_bytes());
        let sum = crc32(&h[..92]);
        h[16..20].copy_from_slice(&sum.to_le_bytes());
        d
    }
    #[test]
    fn recognizes_valid_entries_and_preserves_slot() {
        let mut d = fixture();
        let p = partitions(&mut d).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!((p[0].number, p[0].first_lba, p[0].last_lba), (2, 8, 20));
    }
    #[test]
    fn rejects_corrupted_checksums() {
        let mut d = fixture();
        d.0[512 + 24] ^= 1;
        assert!(partitions(&mut d).unwrap_err().contains("header checksum"));
        let mut d = fixture();
        d.0[1024 + 130] ^= 1;
        assert!(partitions(&mut d).unwrap_err().contains("table checksum"));
    }
    #[test]
    fn skips_invalid_entry_and_keeps_other_partitions() {
        let mut d = fixture();
        // Add an invalid first entry; the valid second entry must still be found.
        d.0[1024] = 1;
        d.0[1024 + 32..1024 + 40].copy_from_slice(&63u64.to_le_bytes());
        d.0[1024 + 40..1024 + 48].copy_from_slice(&64u64.to_le_bytes());
        let sum = crc32(&d.0[1024..1536]);
        d.0[600..604].copy_from_slice(&sum.to_le_bytes());
        d.0[528..532].fill(0);
        let sum = crc32(&d.0[512..604]);
        d.0[528..532].copy_from_slice(&sum.to_le_bytes());
        let entries = partitions(&mut d).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!((entries[0].number, entries[0].first_lba), (2, 8));
    }
}
