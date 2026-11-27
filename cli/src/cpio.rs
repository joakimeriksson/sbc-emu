//! newc cpio writer and the minimal busybox initramfs used for boot tests.

struct Cpio { buf: Vec<u8>, ino: u32 }

impl Cpio {
    fn entry(&mut self, name: &str, mode: u32, data: &[u8], rdev: (u32, u32)) {
        self.ino += 1;
        let hdr = format!(
            "070701{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
            self.ino, mode, 0, 0, 1, 0, data.len(), 0, 0, rdev.0, rdev.1, name.len() + 1, 0
        );
        self.buf.extend_from_slice(hdr.as_bytes());
        self.buf.extend_from_slice(name.as_bytes());
        self.buf.push(0);
        while self.buf.len() % 4 != 0 { self.buf.push(0); }
        self.buf.extend_from_slice(data);
        while self.buf.len() % 4 != 0 { self.buf.push(0); }
    }
    fn dir(&mut self, n: &str) { self.entry(n, 0o040755, &[], (0, 0)); }
    fn file(&mut self, n: &str, mode: u32, d: &[u8]) { self.entry(n, 0o100000 | mode, d, (0, 0)); }
    fn symlink(&mut self, n: &str, target: &str) { self.entry(n, 0o120777, target.as_bytes(), (0, 0)); }
    fn chardev(&mut self, n: &str, major: u32, minor: u32) { self.entry(n, 0o020600, &[], (major, minor)); }
}

const INIT: &str = r#"#!/bin/sh
/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev 2>/dev/null
T=/dev/ttyAMA0
I=/dev/ttyAMA0
if grep -q sbcemu.shell=fb /proc/cmdline; then
  # the shell runs on the screen's console; the USB keyboard types into it
  T=/dev/tty1
  I=/dev/tty1
fi
{
echo
echo "sbc-emu: Linux $(uname -r) on $(cat /proc/device-tree/model 2>/dev/null)"
echo "sbc-emu: busybox shell. 'poweroff -f' to stop."
} >$T
exec setsid -c /bin/sh -l <$I >$T 2>&1
"#;

pub fn busybox_initramfs(busybox: &[u8]) -> Vec<u8> {
    initramfs(busybox, INIT, "pi4# ")
}

/// The Pi 3's console is the mini UART (ttyS0).
pub fn pi3_initramfs(busybox: &[u8]) -> Vec<u8> {
    initramfs(busybox, &INIT.replace("ttyAMA0", "ttyS0"), "pi3# ")
}

pub fn orangepi_initramfs(busybox: &[u8]) -> Vec<u8> {
    initramfs(busybox, &INIT.replace("ttyAMA0", "ttyS2"), "orangepi5# ")
}

fn initramfs(busybox: &[u8], init: &str, prompt: &str) -> Vec<u8> {
    let mut c = Cpio { buf: Vec::new(), ino: 0 };
    for d in ["bin", "sbin", "dev", "proc", "sys", "tmp", "etc", "root"] { c.dir(d); }
    c.chardev("dev/console", 5, 1);
    c.chardev("dev/null", 1, 3);
    c.file("bin/busybox", 0o755, busybox);
    c.symlink("bin/sh", "busybox");
    c.file("init", 0o755, init.as_bytes());
    c.file("etc/profile", 0o644, format!("export PS1='{prompt}'\nexport PATH=/bin:/sbin\n").as_bytes());
    c.entry("TRAILER!!!", 0, &[], (0, 0));
    c.buf
}
