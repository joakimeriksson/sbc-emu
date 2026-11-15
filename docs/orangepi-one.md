# Orange Pi One (Allwinner H3)

`--board orangepi-one`, experimental.

A 32-bit machine on its own ARMv7 core (`arm-a32`, interpreter only): four Cortex-A7 cores with VFPv4 and NEON (started through PSCI CPU_ON, as U-Boot's secure monitor does on the board), the H3's GIC-400 with inter-processor interrupts, UART0
(DesignWare), CCU, timers and the architectural timers. It boots an unmodified Alpine Linux
kernel (6.18 `linux-lts`, armv7) with the unmodified mainline `sun8i-h3-orangepi-one.dtb`:
like U-Boot on the real board, the emulator fills in `/chosen` and `/memory` and adds a
`/psci` node whose calls (CPU on/off, power-off, reset) the host answers. The initramfs is Alpine's
armhf minirootfs, so the shell is an ordinary hard-float BusyBox userland.

```sh
scripts/orangepi-one-fixture.sh            # pinned Alpine downloads -> cache/orangepi-one/
F=cache/orangepi-one
sbc-emu run --board orangepi-one --kernel $F/zImage --dtb $F/sun8i-h3-orangepi-one.dtb \
    --initrd $F/initramfs.cpio.gz           # Ctrl-A x quits
# an SD card image: --sd card.img (copy-on-write, survives guest reboot; --sd-persist writes the file)
```

The NEON check (Debian armhf glibc, OpenSSL and libjpeg-turbo; about 80 s on an M-series Mac):

```sh
scripts/debian-armhf-neon-fixture.sh       # pinned Debian 13 packages -> cache/debian-armhf-neon/
sbc-emu run --board orangepi-one --kernel $F/zImage --dtb $F/sun8i-h3-orangepi-one.dtb \
    --initrd cache/debian-armhf-neon/initramfs.cpio.gz --script scripts/orangepi-one-neon.txt
```

| | Status | Evidence |
|---|---|---|
| Boot | kernel to the `orangepi-one#` BusyBox prompt on four cores (`smp: Brought up 1 node, 4 CPUs`) in 0.67 s of guest time (about 1 G instructions, ~23 s on an M-series Mac) | `scripts/orangepi-one-shell.txt` |
| SMP | `nproc` = 4; four parallel shell loops; rescheduling and function-call IPIs on all four CPUs in `/proc/interrupts` | same script |
| Userland | shell input on `ttyS0`, `/proc/device-tree/model` = `Xunlong Orange Pi One`, VFPv4 floating point (`awk` arithmetic), `sleep`, tmpfs I/O | same script |
| Firmware | PSCI 1.0 over SMC from the host: `reboot -f` boots again, `poweroff -f` ends the run | same script |
| SD card | the H3's SD/MMC controller with its internal DMA (`sunxi-mmc`): a host-made 64 MiB ext4 card mounts; a 4 MiB file written by the guest verifies after dropping the page cache (read back from the card); `fstrim` (erase commands); the data is still there after a guest reboot | `scripts/orangepi-one-sd.txt` |
| Thumb-2 userland | Debian 13's armhf `busybox-static` (Thumb-2, hard float): SHA-256 and MD5 of known strings, `sort`, floating-point `awk`, a gzip round trip of 256 KiB, on four cores | `scripts/debian-armhf-busybox-fixture.sh`, `scripts/orangepi-one-thumb.txt` |
| NEON | Linux sets `HWCAP_NEON` (`neon` in `/proc/cpuinfo`); Debian 13's armhf glibc, OpenSSL 3.5 (`OPENSSL_armcap=0x1`) and libjpeg-turbo 2.1 run their NEON code: SHA-1/256/512, ChaCha20 and Poly1305 of 1.2 MB, a JPEG encode and decode, all equal to the host's native OpenSSL 3 / libjpeg-turbo 3 results and to the guest's scalar paths | `scripts/debian-armhf-neon-fixture.sh`, `scripts/orangepi-one-neon.txt` |

Not modelled yet: Ethernet (EMAC), USB and HDMI/display. Booting from the SD card (U-Boot or an Armbian image) is the next step. The cores
take turns on one host thread (round-robin quanta of 1000 instructions). Every other device node in the
distribution DTB is a plain read-back register file, so its driver probes without effect;
the run summary lists the pages that were touched. Virtual time is 1 ns per instruction.


Design notes, fixture hashes and the bugs found during bring-up: [multi-board-foundation.md](multi-board-foundation.md#orange-pi-one-allwinner-h3-2026-11-15).
