//! The ext4 reader on small images made by `mke2fs -t ext4|ext2 -b 1024 -d DIR IMG 2048`
//! (DIR: boot/armbianEnv.txt, boot/vmlinuz-1 (300 KiB pattern), symlinks boot/zImage ->
//! vmlinuz-1, boot/rel -> ../etc/os-release, boot/abs -> /boot/vmlinuz-1, etc/os-release).
//! The ext2 image uses the classic block map (300 one-KiB blocks: single and double
//! indirect), the ext4 image extents.
use emu_core::disk::MemDisk;
use emu_core::ext4::Ext4;

fn image(compressed: &[u8]) -> MemDisk {
    MemDisk(miniz_oxide::inflate::decompress_to_vec(compressed).unwrap())
}

fn check(mut disk: MemDisk) {
    let mut fs = Ext4::open(&mut disk, 0).unwrap();
    assert_eq!(fs.uuid_string(), "01234567-89ab-cdef-0123-456789abcdef");
    let mut names = fs.list("/boot").unwrap();
    names.sort();
    assert_eq!(names, [".", "..", "abs", "armbianEnv.txt", "dtb", "rel", "vmlinuz-1", "zImage"]);
    let expected: Vec<u8> = (0..300 * 1024).map(|i: usize| (i * 7 + 3) as u8).collect();
    assert_eq!(fs.read_file("/boot/vmlinuz-1").unwrap(), expected);
    assert_eq!(fs.read_file("/boot/zImage").unwrap(), expected, "relative symlink");
    assert_eq!(fs.read_file("/boot/abs").unwrap(), expected, "absolute symlink");
    assert_eq!(fs.read_file("/boot/rel").unwrap(), b"NAME=test\n", "symlink with ..");
    assert_eq!(fs.read_file("/boot/armbianEnv.txt").unwrap(), b"rootdev=UUID=test\n");
    assert!(fs.read_file("/boot/missing").is_err());
    assert!(fs.read_file("/boot").is_err(), "a directory is not a file");
    assert!(fs.exists("/etc/os-release") && !fs.exists("/nope"));
}

#[test]
fn reads_ext4_with_extents() {
    check(image(include_bytes!("data/ext4-small.img.deflate")));
}

#[test]
fn reads_ext2_with_indirect_blocks() {
    check(image(include_bytes!("data/ext2-small.img.deflate")));
}

#[test]
fn rejects_other_filesystems() {
    let mut disk = MemDisk::new(64);
    assert!(Ext4::open(&mut disk, 0).is_err());
}
