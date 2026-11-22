# NVIDIA Jetson Nano (Tegra X1): plan and status

The Jetson Nano Developer Kit (P3450) has NVIDIA's Tegra X1 (T210): four Cortex-A57 cores,
a Maxwell GPU (GM20B, 128 cores), 4 GiB of LPDDR4, a microSD card, four USB 3 ports behind
Tegra's XUSB controller, Gigabit Ethernet (a Realtek RTL8111 on PCIe) and HDMI. The same SoC
is in the first Nintendo Switch.

The aim is the project's usual one: the board's stock software, unmodified, on register-level
models of its hardware. **Mainline Linux first** (open drivers end to end, nouveau and Mesa
for the GPU); NVIDIA's own image (JetPack 4.6: Ubuntu 18.04, NVIDIA's 4.9 kernel and closed
GPU userspace) is a later stretch goal, because it needs NVIDIA's bootloader hand-off, more
of the hardware, and a GPU stack without source.

## Status

**Milestone 1 is done: mainline Linux boots to a shell on all four cores.**

```sh
sh scripts/jetson-nano-fixture.sh cache/jetson-nano      # pinned Alpine aarch64 packages
./target/release/sbc-emu run --board jetson-nano --kernel cache/jetson-nano/vmlinuz \
    --dtb cache/jetson-nano/tegra210-p3450-0000.dtb --initrd cache/jetson-nano/initramfs.cpio.gz
# add --script scripts/jetson-nano-shell.txt --timeout 120 for the automated check
```

- Alpine's `linux-lts` 6.18.64 for aarch64 (an EFI zboot `vmlinuz`, unpacked as the EFI stub
  would) with its unmodified `tegra210-p3450-0000.dtb`; `/memory` and `/chosen` are patched as
  a bootloader patches them.
- Four Cortex-A57s (MIDR `0x411fd071`) brought up by PSCI `CPU_ON`, 4 GiB at 0x8000_0000.
- Each core ticks on its own Tegra timer (TMR10-13, SPIs 176-179), as Linux chooses on this
  SoC; the console is the Tegra UART driver's `ttyS0`.
- Shell after 3.2 s of virtual time and 3.3 s of wall time on an M-series Mac (JIT).
- Linux's remaining complaints: no PSCI `CPU_SUSPEND` (CPU idle states), no PMU.

### What is modelled (`tegra210/`)

| Block | Model |
|---|---|
| Cortex-A57 ×4 | `arm-a64` with the A57's MIDR; RAM at 0x8000_0000 (the core gained `ram_base`) |
| GIC-400 | `peripherals::gic` (GICv2) |
| Legacy interrupt controller (`ictlr`) | read-back registers: interrupts go straight to the GIC |
| Timers (TMR, TIMERUS) | per-core countdown timers with interrupts, the microsecond counter; the count rate comes from `TIMERUS_USEC_CFG` |
| Clock and reset controller | 38.4 MHz oscillator, clock M 19.2 MHz, PLL lock bits, enable/reset set-clear registers, super clocks in RUN |
| PMC | power-gate toggles, I/O pad deep power-down |
| Fuses, chip ID | HIDREV = T210 A02; speedo, SKU and IDDQ fuses |
| Memory controller | the client DMA-flush handshake |
| UART A | `peripherals::uart` (16550) |
| Everything else in 0x0100_0000-0x8000_0000 | read-back registers; first-touched pages are listed at exit (`SBC_STUB_LOG=1` prints them as they happen) |

PSCI is answered by the host, in place of the Nano's secure monitor (TF-A).

## Milestones

1. **Serial shell** (done): CPU, GIC, UART, timers, CAR, PMC, fuses.
2. **Storage and a full OS**: SD card (`sdhci-tegra`: SDHCI plus Tegra's pad calibration and
   tap tuning registers; the existing SD card model), the MAX77620 PMIC on I²C5 (regulators
   for the card's I/O voltage), GPIO, pin mux; boot a full arm64 distribution's root file
   system from the card. PSCI `CPU_SUSPEND`.
3. **USB and network**: XUSB (the driver uploads NVIDIA's firmware, `nvidia/tegra210/xusb.bin`,
   to a falcon microcontroller: accept it and present the xHCI behind it, reusing the Pi 4's
   xHCI model) and the pad controller; then the PCIe controller and an RTL8111 (r8169), or
   USB networking first. Alpine's kernel has no Tegra PCIe driver; Debian's does.
4. **Display**: host1x (the command engine the display and the GPU sit behind), the display
   controllers, SOR/HDMI; `tegra-drm`.
5. **GPU** (GM20B through nouveau and Mesa): the falcons (PMU, FECS, GPCCS) at a high level,
   accepting NVIDIA's signed firmware from `linux-firmware` and reporting it running; the GR
   engine's command processing; Maxwell's shader instruction set (documented by nouveau,
   Mesa's compiler and envytools). On the scale of the Mali-G610 and V3D work.
6. **Browser**: the board page, like the Orange Pi One's (the WebAssembly build already has the
   A64 JIT).

Stretch: NVIDIA's JetPack 4.6 image, booted from its `/boot/extlinux/extlinux.conf`: the
bootloader's hand-off (EEPROM board ID, DT overlays, carve-outs, command line), the vendor
kernel's extra hardware (EMC training, DVFS, thermal), and the closed GPU userspace. NVIDIA's
binaries are not redistributed by this project: visitors would bring their own image.

## Notes

- The stock DT routes most interrupts through `ictlr` (`nvidia,tegra210-ictlr`), whose
  driver only masks and unmasks; the model delivers device interrupts to the GIC directly.
- Tegra210's clock driver `BUG()`s on super clocks that are in neither the RUN nor the IDLE
  state, and on a PLLE reference not in its frequency table: the CAR starts as a bootloader
  leaves it (PLL reference = the oscillator, the CPU on PLLX, the system clock on PLLP).
- 32-bit user programs (Alpine's kernel has `COMPAT`): the Nano's RAM runs from 0x8000_0000 to
  0x1_8000_0000, so `arm-a32` carries 64-bit physical addresses from translation to the bus; its
  JIT keys are RAM offsets under a 64-bit kernel (`ram_phys`, `ram_key`). Checked with Alpine's
  static armhf BusyBox (shell loops; four parallel `md5sum`s equal to the 64-bit ones) and a
  32-bit program that maps a device page through `/dev/mem`, with the JIT and without.
- In Alpine's kernel the Tegra UART driver (`8250_tegra`) is a module, so the fixture's
  `/init` loads it before it opens the console, as any initramfs for this kernel must.
