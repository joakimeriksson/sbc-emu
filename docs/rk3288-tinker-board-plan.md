# Plan: Rockchip RK3288 and the ASUS Tinker Board

Status: proposal (2026-11-15). Nothing here is implemented. It follows the project's own-core
decision (pure Rust, no QEMU) and the multi-board plans in `docs/multi-board-refactoring-plan.md`
and `docs/multi-board-preparation.md`; where they disagree, those documents govern the shared
crates and this one only the RK3288.

## Why this board

- **The RK3288 is the panel board's SoC.** The vendor's first device generation is widely reported
  to use it (forum and spec-list evidence, no primary teardown yet). A faithful RK3288 is the
  base for a panel board profile later (firmware research without bricking hardware).
- **It is a well-documented, open target first.** Mainline Linux has device trees for the ASUS
  Tinker Board / Tinker Board S, Firefly-RK3288, Radxa Rock 2 and MQMaker MiQi, and Armbian
  ships images. The success criterion is the same as for the Pi 4: an unmodified image boots
  on a model of the real chip.
- **The 32-bit core is shared.** The Orange Pi One work (Allwinner H3, Cortex-A7) is starting
  `arm-a32`; the RK3288's Cortex-A17 is the same ARMv7-A architecture.

Board: **ASUS Tinker Board S** (adds eMMC; otherwise the Tinker Board). Image: Armbian for
Tinker Board on a mainline kernel (exact release and sha256 recorded when first booted).

## The hardware to model

| Block | RK3288 / Tinker Board | Linux driver | Reuse |
|---|---|---|---|
| CPU | 4x Cortex-A17, ARMv7-A, Thumb-2, VFPv4 + NEON, LPAE, generic timer | — | `arm-a32` (Codex), extended |
| Interrupts | GIC-400 (GICv2) | `arm,gic-400` | Pi 4's GIC (same IP) |
| SMP bring-up | PMU power domains + boot address in SRAM (no PSCI) | `rockchip,rk3288-smp` | new |
| Clocks, resets | CRU (PLLs, dividers, gates), GRF/SGRF, PMU syscons | `rk3288-cru`, syscon | new; registers that read back what was written, PLLs that report lock |
| Console | UART2, Synopsys DesignWare APB UART (8250) | `snps,dw-apb-uart` | new (small) |
| Timers | arch timer (24 MHz) + Rockchip timers | `arm,armv7-timer`, `rockchip,rk3288-timer` | arch timer from the CPU |
| SD / eMMC / SDIO | Synopsys DesignWare MMC (dw_mshc) with IDMAC | `rockchip,rk3288-dw-mshc` | new; SD card logic from `bcm2711/src/sdhci.rs` |
| PMIC, RTC | RK808 on I²C0 (regulators, RTC, power key) | `rk808` | new I²C device |
| I²C, SPI, GPIO, pinctrl | rk3x I²C, Rockchip SPI, 9 GPIO banks, pinctrl via GRF | `rk3x-i2c`, `rockchip-gpio`, `pinctrl-rockchip` | sensors from the Pi 4 (BME280, MCP3008) |
| Ethernet | GMAC (Synopsys DWMAC) + RTL8211E PHY | `rk3288-gmac` (stmmac) | `sbc-net` NAT, DHCP, port forwarding |
| USB | 2x DWC2 (host + OTG), EHCI/OHCI on host0; on-board hub for the 4 ports | `dwc2`, `ehci-platform` | Pi 4's DWC2, HID keyboard/tablet, mass storage; a **USB hub model** is new |
| Display | VOP (big/little) + Synopsys DW HDMI TX, DDC on I²C | `rockchip-drm`, `dw-hdmi-rockchip` | EDID generator, VNC, browser canvas from the Pi 4 |
| GPU | Mali-T760 MP4 (Midgard) | Panfrost + Mesa | new and large (see M8) |
| Audio | I²S + codec | `rockchip-i2s` | WAV and WebAudio output from the Pi 4 |
| Wi-Fi/BT | RTL8723BS on SDIO | `rtl8723bs` | later; Pi 4's `brcmfmac` work does not carry over |

## Milestones

Each milestone ends with a scripted boot test (`scripts/rk3288-*.txt`) that checks for a stated
output on the serial console, and a README table row with its evidence.

- **M0: board plumbing.** `--board tinkerboard` through the board selection layer from the
  multi-board plan. Board catalogue entry `boards/rockchip/tinkerboard-s`. Depends on Codex's
  foundation work; nothing RK3288-specific merges before it.
- **M1: the 32-bit core, complete enough for Linux.**
  - Instructions: A32 and **Thumb-2** (Debian armhf is built for Thumb-2), VFPv4 and NEON.
  - Memory and system: ARMv7 MMU (short-descriptor and LPAE tables), CP15, generic timer,
    exceptions.
  - Tests: instruction tests in the style of `arm-a64`'s, plus boot tests; the Apple-silicon
    host cannot run A32 code, so expected results come from the architecture manual.
- **M2: kernel to a serial shell.**
  - Boot: mainline `zImage` + `rk3288-tinker-s.dtb` + a busybox initramfs, loaded directly,
    one core.
  - Devices: GIC-400, arch timer, UART2, CRU/GRF/PMU with plausible clocks, SRAM.
  - Pass: `/ #` prompt, `cat /proc/cpuinfo` shows the A17.
- **M3: four cores.** PMU core power-up and the SRAM boot vector the kernel writes; `nproc` = 4
  with `--threads single`, then `--threads multi` once `arm-a32` has the parallel-memory
  semantics `arm-a64` has.
- **M4: storage and a real image.**
  - Devices: dw_mmc for SD and eMMC; RK808 on I²C (Linux will not finish probing the SD
    regulators without it).
  - Boot: firmware-style from the image's `/boot` (`armbianEnv.txt` or `extlinux.conf`, zImage,
    dtb, initrd).
  - Pass: Armbian boots to `login:` from an unmodified SD image.
- **M5: networking.** GMAC + PHY on `sbc-net`: DHCP, DNS, `apt update`, `--hostfwd` for SSH.
- **M6: USB and input.** DWC2 ports and a USB hub model; keyboard, tablet and USB stick.
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
- **Stretch: panel board profile.**
  - Boot: the vendor's own update image (locally, for research; it is not ours to redistribute).
  - Work: decode the pad/encoder control-surface microcontroller protocol, then model the
    touchscreen, display and audio codec.
  - Graphics: the vendor's software likely draws through Arm's Mali blob, so M8 would need to match
    the hardware closely enough for `mali_kbase`, not only Panfrost.

## Order and size

M0–M2 prove the chip; M4 is the first user-visible result (Armbian `login:`). Rough sizes:
- **Small:** M0, M5.
- **Medium:** M2, M3, M6, M7, M10.
- **Large:** M1 (Thumb-2 + NEON breadth), M4 (dw_mmc + clocks + PMIC), M9.
- **Very large:** M8.

M8 and M9 are independent of each other and of M5–M7.

## Risks

- **Thumb-2 and NEON breadth.** Debian userland uses both throughout; gaps show up as random
  SIGILLs. Mitigation: the "interpreter proves it before the JIT emits it" rule from `arm-a64`,
  and a decode-coverage report.
- **Clock tree.** Linux computes MMC, UART and display clocks from CRU registers. Wrong
  dividers mostly matter for MMC timing and HDMI pixel clocks; the model must read back
  consistent PLL state.
- **Documentation.** The RK3288 TRM circulates publicly (v1.1 parts), but gaps will be filled
  from the mainline drivers, as the Pi 4 work did with `vc4`, `v3d` and Mesa.
- **Midgard.** Panfrost documents most of the ISA, but the job chain and tiler formats are
  intricate; budget for it separately and do not let it block M4–M7.
- **Real boot ROM path.** Running U-Boot SPL means modelling DDR controller initialisation.
  Direct `/boot` loading first; the boot ROM + idbloader path is a later fidelity milestone.

## First steps (when M0 lands)

1. Pick and pin the Armbian Tinker Board image; extract `zImage`, the dtb and the initrd with
   `sbc-emu sd ls|cat`.
2. Grow `arm-a32` to Thumb-2 and the ARMv7 MMU with tests (shared with the Orange Pi One).
3. M2 skeleton: memory map from the dtb, GIC-400 from the Pi 4, UART2, CRU/GRF/PMU stubs;
   iterate on the kernel log until the initramfs shell appears.
