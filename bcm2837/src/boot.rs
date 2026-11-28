//! What the Pi 3 firmware does before Linux, done by the emulator: read `config.txt` (its
//! `[pi3]`/`[pi3+]` sections, see `Model::firmware`), load the arm64 Image, the initramfs and a
//! generated device tree, allocate the framebuffer, and start core 0 at the kernel entry with
//! x0 = DTB. The VideoCore boot chain (`bootcode.bin`, `start.elf`) is not executed.
use crate::machine::Machine;

pub const KERNEL_BASE: u64 = 0x20_0000;
pub const DTB: u64 = 0x3f0_0000;
pub const INITRD: u64 = 0x400_0000;

/// For a kernel and initramfs given on the command line: the console on the mini UART.
pub const DEFAULT_CMDLINE: &str = "console=tty1 console=ttyS0,115200 earlycon=uart8250,mmio32,0x3f215040 rdinit=/init loglevel=6";

pub use bcm2711::boot::SdBoot;

/// `config.txt`, `cmdline.txt`, the kernel and its initramfs from the card's boot partition,
/// as the firmware of `m`'s model reads them.
pub fn read_sd_boot(disk: &mut dyn bcm2711::sdhci::Disk, model: crate::Model) -> Result<SdBoot, String> {
    bcm2711::boot::read_sd_boot_for(disk, &model.firmware())
}

pub fn boot_linux(m: &mut Machine, kernel: &[u8], initrd: Option<&[u8]>, cmdline: &str, display: bool) -> Result<(), String> {
    if kernel.len() < 64 || &kernel[56..60] != b"ARMd" {
        return Err("not an uncompressed arm64 Linux Image (gunzip kernel8.img first)".into());
    }
    let linux_ram = m.soc.arm_ram();
    let text_offset = u64::from_le_bytes(kernel[8..16].try_into().unwrap());
    let image_size = u64::from_le_bytes(kernel[16..24].try_into().unwrap());
    let entry = KERNEL_BASE + text_offset;
    if entry + image_size.max(kernel.len() as u64) > DTB { return Err("kernel too large for the load layout".into()); }
    m.load(entry, kernel);
    let initrd_range = match initrd {
        Some(d) => {
            if INITRD + d.len() as u64 > linux_ram / 2 { return Err("initramfs too large".into()); }
            m.load(INITRD, d);
            Some((INITRD, INITRD + d.len() as u64))
        }
        None => None,
    };
    let fb = if display {
        m.soc.mbox.firmware_alloc();
        let f = &m.soc.mbox.fb;
        Some((f.base, f.width, f.height, f.pitch()))
    } else { None };
    // the firmware's `enable_uart=1`: the mini UART is on before Linux (earlycon writes to it)
    m.soc.aux.write(0x04, 1);
    // like the firmware: one 8250 port for the mini UART (Pi kernels allocate none by default)
    let cmdline = if cmdline.contains("8250.nr_uarts") { cmdline.to_string() } else { format!("8250.nr_uarts=1 {cmdline}") };
    let kms = m.soc.vc4.is_some();
    let dtb = crate::fdt::tree(&crate::fdt::BootInfo { model: m.model, ram: linux_ram, bootargs: &cmdline, initrd: initrd_range, fb, cpus: m.cpus.len() as u32,
                                                       kms, v3d: kms && m.has_v3d(), outputs: m.soc.outputs });
    m.load(DTB, &dtb);
    m.boot_linux(entry, DTB);
    Ok(())
}
