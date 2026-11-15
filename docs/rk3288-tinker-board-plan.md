# Plan: Rockchip RK3288 and the ASUS Tinker Board

Status: M2 reached on 2026-11-15 (one core, Debian armmp kernel to a BusyBox shell; see "State"
below). It follows the project's own-core decision (pure Rust, no QEMU, `docs/decisions.md`
D1) and the multi-board foundation in `docs/multi-board-foundation.md`; where they disagree,
that document governs the shared crates and this one only the RK3288.

## Why this board

- **The RK3288 is the panel board's SoC.** Confirmed from the vendor's own update image (firmware 3.10.1 Gen 1):
  `rk3288-panel.dtb` is `model = "RK3288 panel board"`, `compatible = "vendor,panel",
  "vendor,board", "rockchip,rk3288"`. Details in `docs/panel-board-recon.md`.
- **It is a well-documented, open target first.** Mainline Linux has device trees for the ASUS
  Tinker Board / Tinker Board S, Firefly-RK3288, Radxa Rock 2 and MQMaker MiQi, and Armbian
  ships images. The success criterion is the same as for the Pi 4: an unmodified image boots
  on a model of the real chip.
- **The 32-bit core is shared.** The Orange Pi One work (Allwinner H3, Cortex-A7) provides
  `arm-a32`; the RK3288's cores are the same ARMv7-A architecture.

Board: **ASUS Tinker Board S** (adds eMMC; otherwise the Tinker Board). M2 uses Debian 13's
`linux-image-armmp` kernel and the mainline `rk3288-tinker-s.dtb` it ships
(`scripts/tinker-board-fixture.sh`; Alpine's armv7 `linux-lts`, the Orange Pi One kernel, is
built without `ARCH_ROCKCHIP` and has no rk3288 device trees). The image for M4 is Armbian for
Tinker Board on a mainline kernel (exact release and sha256 recorded when first booted).

## The hardware to model

| Block | RK3288 / Tinker Board | Linux driver | Reuse |
|---|---|---|---|
| CPU | 4x Cortex-A12 r0p1 (MIDR 0x410fc0d1; Arm renamed the A12 the Cortex-A17, part 0xc0e), ARMv7-A, Thumb-2, VFPv4 + NEON, LPAE, generic timer | DT `arm,cortex-a12`, `arm,cortex-a12-pmu` | `arm-a32` (Orange Pi One), extended |
| Interrupts | GIC-400 (GICv2) at 0xffc01000 | `arm,gic-400` | `allwinner_h3::Gic` (in use), Pi 4's GIC for SGIs/multi-CPU |
| SMP bring-up | `enable-method = "rockchip,rk3066-smp"`: PMU power domains, CRU core resets, boot ROM mailbox in SRAM (no PSCI) | `arch/arm/mach-rockchip/platsmp.c` | new (see M3) |
| Clocks, resets | CRU (rk3066-type PLLs, hiword-masked dividers and gates), GRF/SGRF, PMU syscons | `rk3288-cru`, syscon | `rockchip-rk3288::cru` (PLLs with lock in GRF_SOC_STATUS1) |
| Console | UART2 at 0xff690000, Synopsys DesignWare APB UART (8250) | `snps,dw-apb-uart` | `allwinner_h3::Uart` (in use) |
| Timers | arch timer (24 MHz; DT `arm,cpu-registers-not-fw-configured`, so Linux uses the secure physical timer, PPI 13) + Rockchip timers (Linux restarts timer7 itself) | `arm,armv7-timer`, `rockchip,rk3288-timer` | arch timer from the CPU |
| SD / eMMC / SDIO | Synopsys DesignWare MMC (dw_mshc) with IDMAC | `rockchip,rk3288-dw-mshc` | new; SD card logic from `bcm2711/src/sdhci.rs` |
| PMIC, RTC | RK808 on I²C0 (regulators, RTC, power key; the system power controller) | `rk808` | new I²C device |
| I²C, SPI, GPIO, pinctrl | rk3x I²C, Rockchip SPI, 9 GPIO banks, pinctrl via GRF | `rk3x-i2c`, `rockchip-gpio`, `pinctrl-rockchip` | sensors from the Pi 4 (BME280, MCP3008) |
| Ethernet | GMAC (Synopsys DWMAC) + RTL8211E PHY | `rk3288-gmac` (stmmac) | `sbc-net` NAT, DHCP, port forwarding |
| USB | 2x DWC2 (host + OTG), EHCI/OHCI on host0; on-board hub for the 4 ports | `dwc2`, `ehci-platform` | Pi 4's DWC2, HID keyboard/tablet, mass storage; a **USB hub model** is new |
| Display | VOP (big/little) + Synopsys DW HDMI TX, DDC on I²C | `rockchip-drm`, `dw-hdmi-rockchip` | EDID generator, VNC, browser canvas from the Pi 4 |
| GPU | Mali-T760 MP4 (Midgard) | Panfrost + Mesa | new and large (see M8) |
| Audio | I²S carries **HDMI audio only** (`simple-audio-card` with the HDMI codec in `rk3288-tinker.dtsi`); the 3.5 mm jack is a Realtek **ALC4040 USB codec** behind the on-board hub (ASUS spec sheet; not in the device tree) | `rockchip-i2s` + `dw-hdmi` audio; `snd-usb-audio` | USB audio class device on the DWC2/hub model; WAV and WebAudio output from the Pi 4 |
| Wi-Fi/BT | RTL8723BS on SDIO | `rtl8723bs` | later; Pi 4's `brcmfmac` work does not carry over |

Verified against mainline (torvalds/linux master, 2026-11-15): `rk3288.dtsi` (CPU compatible,
enable-method, SMP SRAM, timer, UART2, GIC), `arch/arm/mach-rockchip/platsmp.c` (secondary
start), `arch/arm/mach-rockchip/rockchip.c` (timer7 restart), `drivers/clk/rockchip/clk-rk3288.c`
and `clk-pll.c` (PLL layout and lock bits), `drivers/pmdomain/rockchip/pm-domains.c` (power
domains), `rk3288-tinker.dtsi` (audio). The r0p1 revision is what RK3288 silicon reports (stated
in the 2014 linux-arm-kernel review of the Cortex-A12 erratum 818325 workaround); the emulator
uses it and the Debian kernel prints `CPU: ARMv7 Processor [410fc0d1] revision 1`.

## Milestones

Each milestone ends with a scripted boot test (`scripts/tinkerboard-*.txt`) that checks for a
stated output on the serial console, and a README table row with its evidence.

- **M0: board plumbing.** `--board tinkerboard` through `sbc-machine`'s board selection
  (`Board::TinkerBoard`, `cli/src/tinkerboard.rs` on the shared `serial_board` runner). Done.
- **M1: the 32-bit core, complete enough for Linux.** `arm-a32` boots the Debian armmp kernel
  in ARM mode already. Still missing for the RK3288 (owned by the Orange Pi One session; the
  list is in "Core changes needed" below): Thumb-2, NEON, LPAE, A12/A17 ID and cache
  registers, the CP15 c15 errata registers, SMP (SGIs, exclusives across cores).
- **M2: kernel to a serial shell.** Done (2026-11-15).
  - Boot: Debian `zImage` + stock `rk3288-tinker-s.dtb` + a BusyBox initramfs, loaded
    directly at U-Boot's rk3288 addresses, one core, `nosmp`.
  - Devices: GIC-400, arch timer, UART2, CRU/GRF/PMU with plausible clocks; everything else in
    0xff000000-0xffffffff (SRAM, GPIO, QoS, other UARTs) is a read-back stub.
  - Pass (`scripts/tinkerboard-shell.txt`): the `tinker-board#` prompt answers input, the model
    reads "Rockchip RK3288 Asus Tinker Board S", `/proc/cpuinfo` shows **`CPU part: 0xc0d`**
    (Cortex-A12; the kernel names the PMU `armv7_cortex_a12`), VFP userland, timer sleep, tmpfs,
    and `reboot -f` through the CRU global soft reset.
- **M3: four cores.** Mainline `rockchip,rk3066-smp` on a non-A9 core:
  1. `smp_prepare_cpus` reads L2CTLR (`mrc p15, 1, rX, c9, c0, 2`, bits 25:24 + 1) for the core
     count, maps the `rockchip,rk3066-smp-sram` node (first 16 bytes of `sram@ff700000`) and
     the PMU syscon, gets each core's reset from the CRU (`resets` in the cpu nodes), and powers
     cores 1-3 down (PMU_PWRDN_CON 0x08 bits 0-3, polled in PMU_PWRDN_ST 0x0c).
  2. `boot_secondary` asserts the core reset, powers the core up, deasserts the reset, waits
     1 ms, writes the physical `secondary_startup` address to SRAM+8 and **0xDEADBEAF** to
     SRAM+4, then `dsb; sev`.
  3. The boot ROM on the released core (not Linux) waits in WFE until SRAM+4 holds the magic
     and jumps to SRAM+8. The model emulates that ROM loop on the host: a powered, out-of-reset
     core with the magic set starts at the SRAM+8 address in SVC mode, MMU off.
  `nproc` = 4 with `--threads single`, then `--threads multi` once `arm-a32` has the
  parallel-memory semantics `arm-a64` has. The panel board's kernel runs with `isolcpus=2-3` and
  PREEMPT_RT, so its audio engine expects all four cores.
- **M4: storage and a real image.**
  - Devices: dw_mmc for SD and eMMC; RK808 on I²C (Linux will not finish probing the SD
    regulators without it; it is also the system power controller for `poweroff`).
  - Boot: firmware-style from the image's `/boot` (`armbianEnv.txt` or `extlinux.conf`, zImage,
    dtb, initrd).
  - Pass: Armbian boots to `login:` from an unmodified SD image.
- **M5: networking.** GMAC + PHY on `sbc-net`: DHCP, DNS, `apt update`, `--hostfwd` for SSH.
- **M6: USB and input.** DWC2 ports and a USB hub model; keyboard, tablet and USB stick; the
  ALC4040-style USB audio codec for the jack.
- **M7: display.**
  - Devices: VOP + DW HDMI + EDID.
  - Pass: fbcon and DRM/KMS on HDMI; VNC and `--screenshot` work.
- **M8: Mali-T760 (Midgard).**
  - Model: job manager, MMU, tiler and shader cores for the Midgard ISA (VLIW, unlike V3D's QPU).
  - Validation: Panfrost + Mesa, compared pixel by pixel against llvmpipe with the existing
    `tests/gpu/gltest.c` scenes; kmscube and glmark2 as on the Pi 4.
  - Size: the largest milestone; at least the size of the V3D work.
- **M9: speed.**
  - An A32/T32 to A64 JIT; the host cannot execute A32, so this is a real translator, unlike
    `arm-a64`'s same-ISA JIT.
  - Target: Armbian to `login:` in a time comparable to the Pi 4 (about 30 s).
- **M10: browser.** The WebAssembly build and a demo pad on the sbc-emu page, like the Pi 4's.
- **Stretch: panel board profile** (`vendor,panel`; findings in `docs/panel-board-recon.md`).
  - **Kernel.** The vendor's firmware does *not* use the Rockchip 4.4/4.19 BSP: it is a 6.18.44
    PREEMPT_RT kernel with mainline Rockchip drivers plus the vendor board drivers, booted by a
    resident U-Boot (distro boot script, `bootz`). The profile must satisfy *that* kernel's
    drivers and its device tree (`rk3288-panel.dtb`), which differ from the Tinker Board in:
    - PMIC: **ACT8846** on I2C0 (+ SYR827 vdd_cpu, SYR828 vdd_gpu, HYM8563 RTC), not RK808;
    - display: VOP-little -> **DW MIPI DSI** (0xff960000) -> URT UMO-P076MD-T panel rotated 270,
      PWM backlight; HDMI disabled;
    - touch: **Ilitek ILI2116** at 0x41 on I2C4 (out-of-tree driver; a firmware updater runs at
      boot);
    - audio: I2S (8 out / 2 in) to a vendor GPIO-controlled codec (`vendor,panel-audio`),
      not USB;
    - storage: 8-bit eMMC on `dwmmc@ff0f0000` (the root filesystem, A/B-style updates) + SD;
    - Ethernet: GMAC RGMII like the Tinker Board; Wi-Fi/BT described but disabled;
    - USB: EHCI/OHCI host0, DWC2 host1, OTG switchable to gadget (UAC2, MIDI, mass storage);
    - control surface: an **STM32 on USB** (USB MIDI; enters the ST ROM DFU bootloader 0483:df11
      via SysEx for updates). Its USB descriptors and report protocol are the next unknown.
    - CPU errata: the config enables Cortex-A12/A17 errata that write **CP15 c15** registers
      in early setup, which the Debian kernel does not.
  - **Graphics.** The firmware kernel has **Panfrost built in and no `mali_kbase`**; userspace
    is Mesa 24.0.7 with `panfrost_dri.so`. M8 validated against Panfrost is therefore exactly
    what the panel board needs; matching Arm's blob is not required for this firmware.
  - **Userland.** Yocto scarthgap (glibc, systemd). `/usr/bin/app` contains Thumb-2 functions
    (801 of 4706 exported functions have Thumb addresses), so T32 is required; glibc itself is
    ARM-mode.
  - Boot: the vendor's own update image, locally, for research; it is not ours to redistribute and
    nothing from it is committed.

## Order and size

M0–M2 prove the chip; M4 is the first user-visible result (Armbian `login:`). Rough sizes:
- **Small:** M5.
- **Medium:** M3, M6, M7, M10.
- **Large:** M1 (Thumb-2 + NEON breadth), M4 (dw_mmc + clocks + PMIC), M9.
- **Very large:** M8.

M8 and M9 are independent of each other and of M5–M7.

## Reuse from the Orange Pi One (Allwinner H3)

- `arm-a32` (ARMv7 core, short-descriptor MMU, VFP, generic timer) with the identity set by
  the machine (`cpu.midr`, `cpu.mpidr`).
- `allwinner_h3::Gic` (GIC-400) and `allwinner_h3::Uart` (DesignWare APB UART), used through a
  crate dependency until they move to a shared device crate.
- The direct-boot pattern of `allwinner_h3::boot` with `emu_core::fdt::Node` (parse, patch
  `/chosen` and `/memory`, re-serialise); the RK3288 version handles the two-cell root and adds
  no `/psci`.
- `cli/src/serial_board.rs` (scripts, pacing, reset) and the pinned-fixture script pattern.

## State (2026-11-15)

`rockchip-rk3288` crate: `Machine`, `Soc`, `cru` (CRU, GRF lock, PMU), `boot`; probe binary
`rk3288-boot`; `sbc-emu run --board tinkerboard`. The Debian 6.12.107 armmp kernel reaches the
initramfs shell after about 3.7 G instructions (3.7 s virtual, about 80 s on an M-series host).
Unmodelled pages Linux touches on the way: timer 0xff810000, PMU SRAM 0xff720000, DMA
0xff250000/0xffb20000, GPIO banks 0xff750000-0xff7f0000, UART0/1/3/4, QoS 0xffaa0000-0xffaf0000.

## Core changes needed (arm-a32, owned by the Orange Pi One session)

Listed precisely in the session report; in short: Cortex-A12 values for FPSID, CTR, CLIDR,
CCSIDR, PMCR (IDCODE and six counters) and ID_DFR0/DBGDIDR; CP15 c15 diagnostic (op2 1) and
internal feature (op2 2) registers as read/write; Thumb-2 and NEON for userland; LPAE for
`armmp-lpae` kernels; SGIs and per-core state for M3.

## Risks

- **Thumb-2 and NEON breadth.** Debian userland and the vendor application use both; gaps show up
  as random SIGILLs. Mitigation: the "interpreter proves it before the JIT emits it" rule from
  `arm-a64`, and a decode-coverage report.
- **Clock tree.** Linux computes MMC, UART and display clocks from CRU registers. Wrong
  dividers mostly matter for MMC timing and DSI/HDMI pixel clocks; the model must read back
  consistent PLL state (it does for the PLLs; dividers and gates are read-back registers).
- **Documentation.** The RK3288 TRM circulates publicly (v1.1 parts), but gaps will be filled
  from the mainline drivers, as the Pi 4 work did with `vc4`, `v3d` and Mesa.
- **Midgard.** Panfrost documents most of the ISA, but the job chain and tiler formats are
  intricate; budget for it separately and do not let it block M4–M7.
- **Real boot ROM path.** Running U-Boot SPL means modelling DDR controller initialisation.
  Direct `/boot` loading first; the boot ROM + idbloader path is a later fidelity milestone.
  The panel board's U-Boot is not in the update image; the profile will boot its kernel directly.
