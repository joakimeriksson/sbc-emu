//! Read-only FAT16/FAT32 reader for the SD card's boot partition: what the Pi firmware does
//! when it looks for config.txt, cmdline.txt and kernel8.img. Long file names are supported.
use crate::sdhci::Disk;

pub struct Fat<'a> {
    disk: &'a mut dyn Disk,
    part: u64,
    bps: u64,
    spc: u64,
    fat_start: u64,
    data_start: u64,
    root_start: u64,
    root_sectors: u64,
    root_cluster: u32,
    fat32: bool,
}

#[derive(Debug, Clone)]
pub struct Entry { pub name: String, pub dir: bool, pub cluster: u32, pub size: u32 }

/// Partitions from an MBR: (type, first LBA, sectors).
pub fn mbr_partitions(disk: &mut dyn Disk) -> Vec<(u8, u64, u64)> {
    let mut s = [0u8; 512];
    disk.read(0, &mut s);
    if s[510] != 0x55 || s[511] != 0xaa { return Vec::new(); }
    (0..4).filter_map(|i| {
        let e = &s[446 + 16 * i..462 + 16 * i];
        let ty = e[4];
        let lba = u32::from_le_bytes(e[8..12].try_into().unwrap()) as u64;
        let n = u32::from_le_bytes(e[12..16].try_into().unwrap()) as u64;
        (ty != 0).then_some((ty, lba, n))
    }).collect()
}

/// MBR disk identifier (the "PARTUUID" prefix Raspberry Pi OS uses in cmdline.txt).
pub fn mbr_disk_id(disk: &mut dyn Disk) -> u32 {
    let mut s = [0u8; 512];
    disk.read(0, &mut s);
    u32::from_le_bytes(s[440..444].try_into().unwrap())
}

impl<'a> Fat<'a> {
    pub fn open(disk: &'a mut dyn Disk, part: u64) -> Option<Self> {
        let mut b = [0u8; 512];
        disk.read(part, &mut b);
        let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]) as u64;
        let u32at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as u64;
        let bps = u16at(11);
        let spc = b[13] as u64;
        if bps != 512 || spc == 0 { return None; }
        let reserved = u16at(14);
        let nfats = b[16] as u64;
        let root_entries = u16at(17);
        let total = if u16at(19) != 0 { u16at(19) } else { u32at(32) };
        let fatsz = if u16at(22) != 0 { u16at(22) } else { u32at(36) };
        let root_sectors = (root_entries * 32).div_ceil(bps);
        let fat_start = part + reserved;
        let root_start = fat_start + nfats * fatsz;
        let data_start = root_start + root_sectors;
        let clusters = (total - (data_start - part)) / spc;
        let fat32 = clusters >= 65525;
        Some(Fat { disk, part, bps, spc, fat_start, data_start, root_start, root_sectors,
                   root_cluster: if fat32 { u32at(44) as u32 } else { 0 }, fat32 })
    }

    fn next(&mut self, c: u32) -> Option<u32> {
        let (off, w) = if self.fat32 { (c as u64 * 4, 4) } else { (c as u64 * 2, 2) };
        let mut s = [0u8; 512];
        self.disk.read(self.fat_start + off / self.bps, &mut s);
        let o = (off % self.bps) as usize;
        let v = if w == 4 { u32::from_le_bytes(s[o..o + 4].try_into().unwrap()) & 0x0fff_ffff } else { u16::from_le_bytes([s[o], s[o + 1]]) as u32 };
        let end = if self.fat32 { 0x0fff_fff8 } else { 0xfff8 };
        (v >= 2 && v < end).then_some(v)
    }

    fn chain(&mut self, mut c: u32, limit: Option<u64>) -> Vec<u8> {
        let mut out = Vec::new();
        let csize = (self.spc * self.bps) as usize;
        let mut guard = 0;
        while c >= 2 {
            let start = self.data_start + (c as u64 - 2) * self.spc;
            let mut buf = vec![0u8; csize];
            self.disk.read(start, &mut buf);
            out.extend_from_slice(&buf);
            if limit.is_some_and(|l| out.len() as u64 >= l) { break; }
            guard += 1;
            if guard > 1 << 22 { break; }
            match self.next(c) { Some(n) => c = n, None => break }
        }
        if let Some(l) = limit { out.truncate(l as usize); }
        out
    }

    fn dir_bytes(&mut self, cluster: u32) -> Vec<u8> {
        if cluster == 0 && !self.fat32 {
            let mut b = vec![0u8; (self.root_sectors * self.bps) as usize];
            self.disk.read(self.root_start, &mut b);
            b
        } else {
            let c = if cluster == 0 { self.root_cluster } else { cluster };
            self.chain(c, None)
        }
    }

    pub fn list_dir(&mut self, cluster: u32) -> Vec<Entry> {
        let b = self.dir_bytes(cluster);
        let mut out = Vec::new();
        let mut lfn: Vec<(u8, String)> = Vec::new();
        for e in b.chunks_exact(32) {
            if e[0] == 0 { break; }
            if e[0] == 0xe5 { lfn.clear(); continue; }
            let attr = e[11];
            if attr == 0x0f {
                let units: Vec<u16> = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30]
                    .iter().map(|&o| u16::from_le_bytes([e[o], e[o + 1]])).collect();
                let s: String = char::decode_utf16(units.into_iter().take_while(|&u| u != 0 && u != 0xffff)).map(|c| c.unwrap_or('?')).collect();
                lfn.push((e[0] & 0x1f, s));
                continue;
            }
            if attr & 8 != 0 { lfn.clear(); continue; } // volume label
            let name = if !lfn.is_empty() {
                lfn.sort_by_key(|(n, _)| *n);
                lfn.iter().map(|(_, s)| s.as_str()).collect::<String>()
            } else {
                let base = String::from_utf8_lossy(&e[0..8]).trim_end().to_string();
                let ext = String::from_utf8_lossy(&e[8..11]).trim_end().to_string();
                if ext.is_empty() { base } else { format!("{base}.{ext}") }
            };
            lfn.clear();
            let cluster = ((u16::from_le_bytes([e[20], e[21]]) as u32) << 16) | u16::from_le_bytes([e[26], e[27]]) as u32;
            let size = u32::from_le_bytes(e[28..32].try_into().unwrap());
            if name == "." || name == ".." { continue; }
            out.push(Entry { name, dir: attr & 0x10 != 0, cluster, size });
        }
        out
    }

    pub fn find(&mut self, path: &str) -> Option<Entry> {
        let mut cluster = 0u32;
        let mut found = None;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            let e = self.list_dir(cluster).into_iter().find(|e| e.name.eq_ignore_ascii_case(part))?;
            cluster = e.cluster;
            found = Some(e);
        }
        found
    }

    pub fn read_file(&mut self, path: &str) -> Option<Vec<u8>> {
        let e = self.find(path)?;
        if e.dir { return None; }
        if e.size == 0 { return Some(Vec::new()); }
        Some(self.chain(e.cluster, Some(e.size as u64)))
    }

    pub fn partition_lba(&self) -> u64 { self.part }
}
