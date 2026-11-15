//! A read-only ext2/3/4 reader for boot loaders that load the kernel, device tree and
//! initrd from a distribution's root partition (Armbian keeps them in `/boot` on ext4).
//!
//! Supported: extents and the classic direct/indirect block map, 32- and 64-bit group
//! descriptors, linear and hashed (htree, read linearly) directories, fast and block
//! symlinks (absolute and relative). Not supported: inline data, encryption, journals
//! (not needed for reading a cleanly unmounted image).
use crate::disk::Disk;

const MAGIC: u16 = 0xef53;
const INCOMPAT_64BIT: u32 = 0x80;
const EXTENTS_FL: u32 = 0x8_0000;
const INLINE_DATA_FL: u32 = 0x1000_0000;
const S_IFMT: u16 = 0xf000;
const S_IFDIR: u16 = 0x4000;
const S_IFLNK: u16 = 0xa000;

/// MBR partitions: (type, first LBA, sectors).
pub fn mbr_partitions(disk: &mut dyn Disk) -> Vec<(u8, u64, u64)> {
    let mut s = [0u8; 512];
    disk.read(0, &mut s);
    if s[510] != 0x55 || s[511] != 0xaa {
        return Vec::new();
    }
    (0..4)
        .filter_map(|i| {
            let e = &s[446 + 16 * i..462 + 16 * i];
            let lba = u32::from_le_bytes(e[8..12].try_into().unwrap()) as u64;
            let n = u32::from_le_bytes(e[12..16].try_into().unwrap()) as u64;
            (e[4] != 0).then_some((e[4], lba, n))
        })
        .collect()
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

struct Inode {
    mode: u16,
    size: u64,
    flags: u32,
    block: [u8; 60],
}

pub struct Ext4<'a> {
    disk: &'a mut dyn Disk,
    /// First sector of the partition.
    start: u64,
    block_size: u64,
    inodes_per_group: u32,
    inode_size: u64,
    first_data_block: u64,
    desc_size: u64,
    is_64bit: bool,
    /// Filesystem UUID (`root=UUID=...`).
    pub uuid: [u8; 16],
}

impl<'a> Ext4<'a> {
    pub fn open(disk: &'a mut dyn Disk, start_lba: u64) -> Result<Self, String> {
        let mut sb = [0u8; 1024];
        disk.read(start_lba + 2, &mut sb);
        if u16_at(&sb, 56) != MAGIC {
            return Err("not an ext2/3/4 filesystem".into());
        }
        let log = u32_at(&sb, 24);
        if log > 6 {
            return Err("unsupported ext4 block size".into());
        }
        let incompat = u32_at(&sb, 96);
        let is_64bit = incompat & INCOMPAT_64BIT != 0;
        let rev = u32_at(&sb, 76);
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&sb[104..120]);
        Ok(Ext4 {
            disk,
            start: start_lba,
            block_size: 1024 << log,
            inodes_per_group: u32_at(&sb, 40),
            inode_size: if rev >= 1 { u16_at(&sb, 88) as u64 } else { 128 },
            first_data_block: u32_at(&sb, 20) as u64,
            desc_size: if is_64bit { (u16_at(&sb, 254) as u64).max(32) } else { 32 },
            is_64bit,
            uuid,
        })
    }

    /// The UUID in the form `root=UUID=` uses.
    pub fn uuid_string(&self) -> String {
        let h: Vec<String> = self.uuid.iter().map(|b| format!("{b:02x}")).collect();
        format!("{}-{}-{}-{}-{}", h[0..4].concat(), h[4..6].concat(), h[6..8].concat(), h[8..10].concat(), h[10..16].concat())
    }

    fn read_bytes(&mut self, offset: u64, out: &mut [u8]) {
        // Whole sectors around the requested range.
        let first = offset / 512;
        let last = (offset + out.len() as u64).div_ceil(512);
        let mut buf = vec![0u8; ((last - first) * 512) as usize];
        self.disk.read(self.start + first, &mut buf);
        let skip = (offset - first * 512) as usize;
        out.copy_from_slice(&buf[skip..skip + out.len()]);
    }

    fn read_block(&mut self, block: u64) -> Vec<u8> {
        let mut b = vec![0u8; self.block_size as usize];
        let offset = block * self.block_size;
        self.read_bytes(offset, &mut b);
        b
    }

    fn inode(&mut self, ino: u32) -> Result<Inode, String> {
        if ino == 0 {
            return Err("inode 0".into());
        }
        let group = ((ino - 1) / self.inodes_per_group) as u64;
        let index = ((ino - 1) % self.inodes_per_group) as u64;
        let mut desc = vec![0u8; self.desc_size as usize];
        let gdt = (self.first_data_block + 1) * self.block_size;
        self.read_bytes(gdt + group * self.desc_size, &mut desc);
        let mut table = u32_at(&desc, 8) as u64;
        if self.is_64bit && self.desc_size >= 64 {
            table |= (u32_at(&desc, 0x28) as u64) << 32;
        }
        let mut raw = vec![0u8; self.inode_size as usize];
        self.read_bytes(table * self.block_size + index * self.inode_size, &mut raw);
        let mut block = [0u8; 60];
        block.copy_from_slice(&raw[40..100]);
        Ok(Inode {
            mode: u16_at(&raw, 0),
            size: u32_at(&raw, 4) as u64 | (u32_at(&raw, 108) as u64) << 32,
            flags: u32_at(&raw, 32),
            block,
        })
    }

    /// Physical block for logical block `n` (None: a hole or an unwritten extent).
    fn map(&mut self, inode: &Inode, n: u64) -> Result<Option<u64>, String> {
        if inode.flags & EXTENTS_FL != 0 {
            let mut node = inode.block.to_vec();
            for _ in 0..8 {
                if u16_at(&node, 0) != 0xf30a {
                    return Err("bad extent header".into());
                }
                let entries = u16_at(&node, 2) as usize;
                let depth = u16_at(&node, 6);
                if depth == 0 {
                    for i in 0..entries {
                        let e = &node[12 + 12 * i..24 + 12 * i];
                        let first = u32_at(e, 0) as u64;
                        let mut len = u16_at(e, 4) as u64;
                        let unwritten = len > 32768;
                        if unwritten {
                            len -= 32768;
                        }
                        if (first..first + len).contains(&n) {
                            if unwritten {
                                return Ok(None);
                            }
                            let start = (u16_at(e, 6) as u64) << 32 | u32_at(e, 8) as u64;
                            return Ok(Some(start + n - first));
                        }
                    }
                    return Ok(None);
                }
                // Index node: the last entry whose first block <= n.
                let mut child = None;
                for i in 0..entries {
                    let e = &node[12 + 12 * i..24 + 12 * i];
                    if u32_at(e, 0) as u64 <= n {
                        child = Some(u32_at(e, 4) as u64 | (u16_at(e, 8) as u64) << 32);
                    }
                }
                match child {
                    Some(block) => node = self.read_block(block),
                    None => return Ok(None),
                }
            }
            return Err("extent tree too deep".into());
        }
        // Classic block map: 12 direct, then single, double and triple indirect blocks.
        let per = self.block_size / 4;
        let direct = |i: usize| u32_at(&inode.block, 4 * i) as u64;
        if n < 12 {
            return Ok(Some(direct(n as usize)).filter(|&b| b != 0));
        }
        let mut n = n - 12;
        let mut level_size = 1;
        for (level, slot) in [(1, 12), (2, 13), (3, 14)] {
            level_size *= per;
            if n < level_size {
                let mut block = direct(slot);
                let mut span = level_size;
                for _ in 0..level {
                    if block == 0 {
                        return Ok(None);
                    }
                    span /= per;
                    let table = self.read_block(block);
                    block = u32_at(&table, ((n / span) % per * 4) as usize) as u64;
                }
                return Ok(Some(block).filter(|&b| b != 0));
            }
            n -= level_size;
        }
        Err("file too large for the block map".into())
    }

    fn read_inode_data(&mut self, inode: &Inode) -> Result<Vec<u8>, String> {
        if inode.flags & INLINE_DATA_FL != 0 {
            return Err("inline data is not supported".into());
        }
        if inode.mode & S_IFMT == S_IFLNK && inode.size < 60 && inode.flags & EXTENTS_FL == 0 {
            return Ok(inode.block[..inode.size as usize].to_vec()); // fast symlink
        }
        let mut data = Vec::with_capacity(inode.size as usize);
        let blocks = inode.size.div_ceil(self.block_size);
        for n in 0..blocks {
            match self.map(inode, n)? {
                Some(block) => data.extend_from_slice(&self.read_block(block)),
                None => data.resize(data.len() + self.block_size as usize, 0),
            }
        }
        data.truncate(inode.size as usize);
        Ok(data)
    }

    fn lookup_in(&mut self, dir: u32, name: &str) -> Result<Option<u32>, String> {
        let inode = self.inode(dir)?;
        if inode.mode & S_IFMT != S_IFDIR {
            return Err("not a directory".into());
        }
        let data = self.read_inode_data(&inode)?;
        let mut pos = 0;
        while pos + 8 <= data.len() {
            let ino = u32_at(&data, pos);
            let rec_len = u16_at(&data, pos + 4) as usize;
            let name_len = data[pos + 6] as usize;
            if rec_len < 8 {
                break;
            }
            if ino != 0 && pos + 8 + name_len <= data.len() && &data[pos + 8..pos + 8 + name_len] == name.as_bytes() {
                return Ok(Some(ino));
            }
            pos += rec_len;
        }
        Ok(None)
    }

    /// Resolve `path` (absolute) to an inode number, following symlinks.
    fn resolve(&mut self, path: &str, depth: u32) -> Result<u32, String> {
        if depth > 16 {
            return Err("too many symlinks".into());
        }
        let mut stack: Vec<u32> = vec![2];
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
        for (i, part) in parts.iter().enumerate() {
            if *part == ".." {
                if stack.len() > 1 {
                    stack.pop();
                }
                continue;
            }
            let dir = *stack.last().unwrap();
            let ino = self.lookup_in(dir, part)?.ok_or_else(|| format!("{path}: no such file"))?;
            let inode = self.inode(ino)?;
            if inode.mode & S_IFMT == S_IFLNK {
                let target = String::from_utf8_lossy(&self.read_inode_data(&inode)?).into_owned();
                let base = if target.starts_with('/') {
                    String::new()
                } else {
                    format!("/{}", parts[..i].join("/"))
                };
                let rest = parts[i + 1..].join("/");
                let full = format!("{base}/{target}/{rest}");
                return self.resolve(&full, depth + 1);
            }
            stack.push(ino);
        }
        Ok(*stack.last().unwrap())
    }

    /// The contents of the file at `path`.
    pub fn read_file(&mut self, path: &str) -> Result<Vec<u8>, String> {
        let ino = self.resolve(path, 0)?;
        let inode = self.inode(ino)?;
        if inode.mode & S_IFMT == S_IFDIR {
            return Err(format!("{path}: is a directory"));
        }
        self.read_inode_data(&inode)
    }

    pub fn exists(&mut self, path: &str) -> bool {
        self.resolve(path, 0).is_ok()
    }

    /// Names in the directory at `path`.
    pub fn list(&mut self, path: &str) -> Result<Vec<String>, String> {
        let ino = self.resolve(path, 0)?;
        let inode = self.inode(ino)?;
        let data = self.read_inode_data(&inode)?;
        let mut names = Vec::new();
        let mut pos = 0;
        while pos + 8 <= data.len() {
            let rec_len = u16_at(&data, pos + 4) as usize;
            let name_len = data[pos + 6] as usize;
            if rec_len < 8 {
                break;
            }
            if u32_at(&data, pos) != 0 && pos + 8 + name_len <= data.len() {
                names.push(String::from_utf8_lossy(&data[pos + 8..pos + 8 + name_len]).into_owned());
            }
            pos += rec_len;
        }
        Ok(names)
    }
}
