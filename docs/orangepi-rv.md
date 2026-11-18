# Orange Pi RV (StarFive JH7110)

`--board orangepi-rv`, experimental.

Xunlong's Orange Pi RV: the StarFive JH7110 with four SiFive U74 cores (RV64GC, Sv39) and
an S7 monitor core, 4 GiB of LPDDR4, a microSD slot, SPI NOR, Gigabit Ethernet, four USB
3.0 ports (a VL805 on PCIe), HDMI 2.0 and an M.2 slot. sbc-emu runs it on its own RV64
interpreter (`riscv64/`, no QEMU) and SoC model (`starfive-jh7110/`). The Linux kernel and
device tree are unmodified Alpine packages: the mainline `jh7110-orangepi-rv.dtb` (in the
kernel since 7.0) and the generic OpenSBI firmware.

```sh
scripts/orangepi-rv-fixture.sh             # pinned Alpine downloads -> cache/orangepi-rv/
F=cache/orangepi-rv
sbc-emu run --board orangepi-rv --firmware $F/fw_dynamic.bin --kernel $F/Image \
    --dtb $F/jh7110-orangepi-rv.dtb --initrd $F/initramfs.cpio.gz     # Ctrl-A x quits
# --smp 1..4 (default 4), --ram-mb N (default 4096), --append ARGS, --script FILE
```

The boot is the board's own chain from the second stage on. On the board, the mask ROM
loads U-Boot SPL, which trains DRAM and starts OpenSBI's `fw_dynamic` at 0x4000_0000 on
every U74, followed by U-Boot. Here the emulator plays the SPL and U-Boot parts: it loads
OpenSBI at 0x4000_0000 and starts harts 1–4 there with a `fw_dynamic_info` that names
the kernel at 0x4020_0000 (S mode) as the next stage. It loads the device tree at
0x4600_0000 and the initrd at 0x4610_0000 (U-Boot's `starfive_visionfive2` load
addresses), and fills in `/chosen` and `/memory` as U-Boot would. Real OpenSBI then runs:
the platform, HSM, IPIs, the timer, the console and system reset are all OpenSBI's.

## Status

| | Status | Evidence |
|---|---|---|
| CPU | RV64IMAFDC + Zicsr, Zifencei, Zicntr, Zba, Zbb (the U74's `rv64imafdc_zba_zbb`), M/S/U at privileged 1.11, Sv39 with superpages and hardware A/D, no ASIDs; all 166 riscv-tests in the U74's ISA pass (`rv64ui/um/ua/uc/uf/ud/si/mi/uzba/uzbb -p`) | `scripts/riscv-tests.sh`, `cargo test -p riscv64` |
| Firmware | OpenSBI 1.8.1 (generic): `Platform Name: Xunlong Orange Pi RV`, 4 harts, aclint-mswi/mtimer, uart8250, `pm-reset` | `scripts/orangepi-rv-shell.txt` |
| Boot | Alpine linux-stable 7.1.5 to the `orangepi-rv#` BusyBox prompt at about 3.3 s of guest time (about 3.8 G instructions, 30 s wall on an M-series Mac) | same script |
| Shell | serial input and output on `ttyS0` (DW 8250, PLIC interrupts); `uname -a`; `/proc/device-tree/model` = `Xunlong Orange Pi RV`; `/proc/cpuinfo` 4 harts, `rv64imafdc_zicntr_zicsr_zifencei_zihpm_zaamo_zalrsc_zca_zcd_zba_zbb`, `mmu: sv39`; `free -m` 3934 MB; `awk` double-precision math (musl, lp64d) | same script |
| SMP and timer | `smp: Brought up 1 node, 4 CPUs`; `sleep 1`; timer, rescheduling and function-call IPIs on all four CPUs; four parallel shell loops | same script |
| Reset | `reboot -f`: Linux's `gpio-restart` (GPIO35) resets the board and OpenSBI boots again; `poweroff -f`: OpenSBI SRST → AXP15060 PMIC power register over I2C5 → the run ends | `scripts/orangepi-rv-reboot.txt` |

## What is modelled

- `riscv64/`: the hart (interpreter; a fetch page cache and a 1024-entry TLB). Floating
  point uses host IEEE arithmetic with RISC-V NaN boxing and canonical NaNs. The other
  rounding modes and the exception flags come from exact error terms.
- `starfive-jh7110/`:
  - the CLINT (`mtime` at 4 MHz) and the PLIC (136 sources, 9 contexts, level gateways);
  - UART0, the shared DesignWare UART model;
  - the clock/reset generators (`syscrg`, `aoncrg`, `stgcrg`, `ispcrg`, `voutcrg`), whose
    reset status reads back as Linux's driver expects;
  - the PLL syscon (PLL0 1 GHz, PLL1 1064 MHz, PLL2 1188 MHz);
  - the power-management unit's domains;
  - I2C5 (DesignWare, polled) with the AXP15060 PMIC;
  - the GPIO35 restart line.
- Other device pages are read-back stand-ins. Every run lists them at the end.
- Virtual time: 1 ns per instruction per hart, with harts scheduled round-robin in
  1000-instruction slices. When every hart waits in WFI, time skips to the next
  `mtimecmp`.

## Not working yet

- **Storage**: microSD (`mmc@16020000`, DesignWare MMC) and the SDIO Wi-Fi slot are stand-ins:
  `dwmmc_starfive 16020000.mmc: Timeout resetting block`. The RK3288's `dwmmc.rs` is the
  same controller family and is the next step. No SPI NOR (Cadence QSPI).
- **Ethernet** (DWMAC + Motorcomm PHY), **USB 3.0** (PLDA PCIe → VL805 xHCI; the PCIe ports
  report `link down`), **HDMI** (the DC8200 display controller and Inno HDMI; this kernel
  has no driver for them yet), the GPU, audio, M.2: not modelled.
- **Firmware**: no mask ROM, U-Boot SPL or U-Boot. Mainline U-Boot recognises the board
  (EEPROM product ID `XOPIRV`, since 2026.07) and would need the DDR controller, QSPI/SD and
  the I²C EEPROM.
- **Distribution images**: Xunlong's Debian images (vendor kernel 5.15, on Google Drive)
  are not tried yet.
- **Speed**: interpreter only, roughly 125 M instructions per second, all harts on one host
  thread.
