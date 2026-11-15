# Orange Pi 5 experimental direct boot

The `orangepi5` board models one reduced Cortex-A55, the RK3588S GICv3 distributor and redistributor, UART2 and the 24 MHz timer. Firmware services are handled by the host; Linux receives a generated reduced device tree. Only serial direct boot is supported. SD, Ethernet, USB, graphics, wireless, snapshots and the remaining seven CPU cores are not implemented. The advertised CPU features are limited to what the emulator implements.

The following fixture booted Debian 6.12.107 to a BusyBox shell. The Debian installer kernel is from `https://deb.debian.org/debian/dists/trixie/main/installer-arm64/20250803+deb13u7/images/netboot/debian-installer/arm64/linux`, SHA-256 `9c37ace54ef7e1919411fd912e9f34d6982ddd7f4c8ef3c8aee56f4c4d70db67`. A static AArch64 BusyBox is required; the tested Alpine `busybox-static-1.36.1-r31.apk` has SHA-256 `1d8e7a7fc2ed69bdb8eb7be9d962489c21a0f75b72d8a325437c8a09d4cfac70`.

```sh
cargo build --release
./target/release/sbc-emu mkinitramfs --board orangepi5 --busybox /path/to/busybox.static -o /tmp/orangepi-initramfs.cpio
./target/release/sbc-emu run --board orangepi5 --kernel /path/to/Image --initrd /tmp/orangepi-initramfs.cpio
```

Ctrl-A x exits. `scripts/orangepi-shell.txt` automates shell input, timer sleep, RAM filesystem, reboot and power-off with `--script`. Use `--timeout 30` to bound a scripted run. This is a generic Debian kernel and custom initramfs, not an Orange Pi vendor image.
