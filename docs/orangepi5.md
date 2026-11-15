# Orange Pi 5 experimental direct boot

The `orangepi5` board models one reduced Cortex-A55, the RK3588S GICv3 distributor and redistributor, UART2 and the 24 MHz timer. Firmware services are handled by the host; Linux receives a generated reduced device tree. Direct boot supports an optional microSD image through the RK3588S DesignWare MMC controller in PIO mode. Ethernet, USB, graphics, wireless, snapshots and the remaining seven CPU cores are not implemented. The advertised CPU features are limited to what the emulator implements.


## SD root filesystem fixture

A direct-boot initramfs can load the MMC and ext4 modules, mount an ext4 filesystem on the emulated SD card, and switch to its `/sbin/init`. The following reproducible fixture tests that path, including an ext4 write, `sync`, guest reboot and reading the write from the same copy-on-write overlay. The original SD image remains unchanged. This fixture uses a filesystem directly on the card, not a partition table or vendor distribution.

Use the Debian kernel specified above and the modules from the matching `linux-image-6.12.107+deb13-arm64-unsigned_6.12.107-1_arm64.deb` (SHA-256 `2f986ade1a1336423adf2316ea89c3525dd4c05a646b2141626490ca2f388339`). Extract that package to a scratch directory; pass its `usr/lib/modules/6.12.107+deb13-arm64` directory below. The fixture builder requires `mke2fs` and `cpio`, and creates a new output directory rather than overwriting existing images.

```sh
python3 tools/orangepi-sd-root.py --busybox /path/to/busybox.static \
  --modules /path/to/usr/lib/modules/6.12.107+deb13-arm64 \
  --mke2fs /path/to/mke2fs --output out/orangepi-sd-root
./target/release/sbc-emu run --board orangepi5 --kernel /path/to/Image \
  --initrd out/orangepi-sd-root/initramfs.cpio --sd out/orangepi-sd-root/sd-root.img \
  --script scripts/orangepi-sd-root.txt --timeout 60
```

The kernel and initramfs are still loaded directly by the emulator. This does not demonstrate booting an Orange Pi vendor image or loading its bootloader, kernel or initramfs from SD.

The following fixture booted Debian 6.12.107 to a BusyBox shell. The Debian installer kernel is from `https://deb.debian.org/debian/dists/trixie/main/installer-arm64/20250803+deb13u7/images/netboot/debian-installer/arm64/linux`, SHA-256 `9c37ace54ef7e1919411fd912e9f34d6982ddd7f4c8ef3c8aee56f4c4d70db67`. A static AArch64 BusyBox is required; the tested Alpine `busybox-static-1.36.1-r31.apk` has SHA-256 `1d8e7a7fc2ed69bdb8eb7be9d962489c21a0f75b72d8a325437c8a09d4cfac70`.

```sh
cargo build --release
./target/release/sbc-emu mkinitramfs --board orangepi5 --busybox /path/to/busybox.static -o /tmp/orangepi-initramfs.cpio
./target/release/sbc-emu run --board orangepi5 --kernel /path/to/Image --initrd /tmp/orangepi-initramfs.cpio
```

For an SD data disk, add `--sd /path/to/card.img` to the direct-boot command. The base image is opened read-only; guest writes go to a memory overlay that survives guest reboot and is discarded on exit. The kernel must include the DesignWare MMC, Rockchip glue and MMC block drivers, or the initramfs must load them. SD-card boot and persistent image writes are not implemented.

The tested Debian installer initramfs (`20250803+deb13u7/images/netboot/debian-installer/arm64/initrd.gz`, SHA-256 `f8f48fe31f84fca1a43c3d57c09cd1c2ee728549dd18a986463efd0305249451`) contains `dw_mmc.ko.xz`, `dw_mmc-pltfm.ko.xz`, and `dw_mmc-rockchip.ko.xz`. To reproduce the block I/O test, decompress those three modules into the root of the BusyBox initramfs as `/dw_mmc.ko`, `/dw_mmc-pltfm.ko` and `/dw_mmc-rockchip.ko`, create a scratch SD image of at least 1 MiB with `INPUT_SECTOR` at offset 0, then run with `--script scripts/orangepi-sd.txt --timeout 40`. The script loads the modules, reads the image, writes through the guest block device and reads the overlay back after a guest reboot. The model reports a 32-bit PIO interface; DMA, UHS, eMMC and vendor-image boot are not implemented. `SBC_MMC_TRACE=1` logs card commands.

Ctrl-A x exits. `scripts/orangepi-shell.txt` automates shell input, timer sleep, RAM filesystem, reboot and power-off with `--script`. Use `--timeout 30` to bound a scripted run. This is a generic Debian kernel and custom initramfs, not an Orange Pi vendor image.
