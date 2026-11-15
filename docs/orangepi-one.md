# Orange Pi One (Allwinner H3)

`--board orangepi-one`, experimental.

A 32-bit machine on its own ARMv7 core (`arm-a32`, interpreter only): one Cortex-A7 with VFPv4, the H3's GIC-400, UART0
(DesignWare), CCU, timers and the architectural timers. It boots an unmodified Alpine Linux
kernel (6.18 `linux-lts`, armv7) with the unmodified mainline `sun8i-h3-orangepi-one.dtb`:
like U-Boot on the real board, the emulator fills in `/chosen` and `/memory` and adds a
`/psci` node whose calls (power-off, reset) the host answers. The initramfs is Alpine's
armhf minirootfs, so the shell is an ordinary hard-float BusyBox userland.

```sh
scripts/orangepi-one-fixture.sh            # pinned Alpine downloads -> cache/orangepi-one/
F=cache/orangepi-one
sbc-emu run --board orangepi-one --kernel $F/zImage --dtb $F/sun8i-h3-orangepi-one.dtb \
    --initrd $F/initramfs.cpio.gz           # Ctrl-A x quits
```

| | Status | Evidence |
|---|---|---|
| Boot | kernel to the `orangepi-one#` BusyBox prompt in 0.48 s of guest time (about 1 G instructions, ~20 s on an M-series Mac) | `scripts/orangepi-one-shell.txt` |
| Userland | shell input on `ttyS0`, `/proc/device-tree/model` = `Xunlong Orange Pi One`, VFPv4 floating point (`awk` arithmetic), `sleep`, tmpfs I/O | same script |
| Firmware | PSCI 1.0 over SMC from the host: `reboot -f` boots again, `poweroff -f` ends the run | same script |

Not modelled yet: SD/MMC, Ethernet (EMAC), USB, HDMI/display, the second to fourth cores
(the default command line has `nosmp`), Thumb-2 and NEON. Every other device node in the
distribution DTB is a plain read-back register file, so its driver probes without effect;
the run summary lists the pages that were touched. Virtual time is 1 ns per instruction.


Design notes, fixture hashes and the bugs found during bring-up: [multi-board-foundation.md](multi-board-foundation.md#orange-pi-one-allwinner-h3-2026-11-15).
