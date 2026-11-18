# Plan: Rockchip RK3288 and the ASUS Tinker Board

Status (2026-11-18): M0–M4 and M8 (Mali-T760) done, M7 for the DSI panel only, M9 for the
JIT only; the panel board profile runs the vendor's application with touch, Wi-Fi and audio. See "State"
below. M4 (Armbian from an unmodified SD image to a root shell) was reached on 2026-11-17, M3
(four cores) on 2026-11-15. It follows the project's own-core
decision (pure Rust, no QEMU, `docs/decisions.md` D1) and the multi-board foundation in
`docs/multi-board-foundation.md`; where they disagree, that document governs the shared crates
and this one only the RK3288.

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
built without `ARCH_ROCKCHIP` and has no rk3288 device trees). The image for M4 is Armbian
26.8.1 for the Tinker Board, Debian trixie, mainline 6.18.43 `current` kernel, minimal:
`https://dl.armbian.com/tinkerboard/archive/Armbian_26.8.1_Tinkerboard_trixie_current_6.18.43_minimal.img.xz`,
sha256 `cd9a7e3fe6b4b7f25612df33f807674819a736370d65ea4465fd92735024ae18` (the `.sha` Armbian
publishes next to it; `scripts/armbian-tinkerboard-fixture.sh`).

## The hardware to model

| Block | RK3288 / Tinker Board | Linux driver | Reuse |
|---|---|---|---|
| CPU | 4x Cortex-A12 r0p1 (MIDR 0x410fc0d1; Arm renamed the A12 the Cortex-A17, part 0xc0e), ARMv7-A, Thumb-2, VFPv4 + NEON, LPAE, generic timer | DT `arm,cortex-a12`, `arm,cortex-a12-pmu` | `arm-a32` (Orange Pi One), extended |
| Interrupts | GIC-400 (GICv2) at 0xffc01000 | `arm,gic-400` | `allwinner_h3::Gic` (in use), Pi 4's GIC for SGIs/multi-CPU |
| SMP bring-up | `enable-method = "rockchip,rk3066-smp"`: PMU power domains, CRU core resets, boot ROM mailbox in SRAM (no PSCI) | `arch/arm/mach-rockchip/platsmp.c` | `rockchip-rk3288::Machine` (M3; SMP scheduling copied from the H3 machine) |
| Clocks, resets | CRU (rk3066-type PLLs, hiword-masked dividers and gates), GRF/SGRF, PMU syscons | `rk3288-cru`, syscon | `rockchip-rk3288::cru` (PLLs with lock in GRF_SOC_STATUS1) |
| Console | UART2 at 0xff690000, Synopsys DesignWare APB UART (8250) | `snps,dw-apb-uart` | `allwinner_h3::Uart` (in use) |
| Timers | arch timer (24 MHz; DT `arm,cpu-registers-not-fw-configured`, so Linux uses the secure physical timer, PPI 13) + Rockchip timers (Linux restarts timer7 itself) | `arm,armv7-timer`, `rockchip,rk3288-timer` | arch timer from the CPU |
| SD / eMMC / SDIO | Synopsys DesignWare MMC (dw_mshc) 2.70a with IDMAC; SDMMC 0xff0c0000, SDIO0/1 0xff0d0000/0xff0e0000, eMMC 0xff0f0000 | `rockchip,rk3288-dw-mshc` | `rockchip-rk3288::dwmmc` (M4) with `emu_core::sdcard::SdCard` and a new eMMC device (`emmc.rs`) |
| PMIC, RTC | RK808 on I²C0 at 0x1b (regulators, RTC, power key; the system power controller) | `rk808` | `rockchip-rk3288::rk808` (M4) |
| I²C, SPI, GPIO, pinctrl | rk3x I²C, Rockchip SPI, 9 GPIO banks, pinctrl via GRF | `rk3x-i2c`, `rockchip-gpio`, `pinctrl-rockchip` | `rockchip-rk3288::i2c` (six controllers, device trait) and `gpio` (input levels) in M4; sensors from the Pi 4 (BME280, MCP3008) later |
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
  in ARM mode, has Thumb-2, SMP support (per-core state, WFE/SEV) and, since M3, the
  Cortex-A12 identity (`arm_a32::CORTEX_A12`) with the CP15 c15 errata registers, and NEON
  (#18). Still missing (list in "Core changes needed" below): LPAE.
- **M2: kernel to a serial shell.** Done (2026-11-15).
  - Boot: Debian `zImage` + stock `rk3288-tinker-s.dtb` + a BusyBox initramfs, loaded
    directly at U-Boot's rk3288 addresses, one core, `nosmp`.
  - Devices: GIC-400, arch timer, UART2, CRU/GRF/PMU with plausible clocks; everything else in
    0xff000000-0xffffffff (SRAM, GPIO, QoS, other UARTs) is a read-back stub.
  - Pass (`scripts/tinkerboard-shell.txt`): the `tinker-board#` prompt answers input, the model
    reads "Rockchip RK3288 Asus Tinker Board S", `/proc/cpuinfo` shows **`CPU part: 0xc0d`**
    (Cortex-A12; the kernel names the PMU `armv7_cortex_a12`), VFP userland, timer sleep, tmpfs,
    and `reboot -f` through the CRU global soft reset.
- **M3: four cores.** Done (2026-11-15). Mainline `rockchip,rk3066-smp` on a non-A9 core:
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
  The model: `CoreState::BootRom` is a core whose PMU domain is on and whose CRU soft reset
  (SOFTRST_CON0 bits 0-3, `SRST_CORE0..3`, hiword) is released; it checks the mailbox when it
  enters the ROM and on every SEV, as the ROM's WFE loop would. Turning the domain off or
  asserting the reset stops the core (hotplug `cpu_kill`). The cores run in round-robin
  quanta of 1000 instructions on one host thread, as on the Orange Pi One; a parallel
  (`--threads multi`) mode waits for `arm-a32` to get the parallel-memory semantics `arm-a64`
  has. The panel board's kernel runs with `isolcpus=2-3` and PREEMPT_RT, so its audio engine
  expects all four cores.
  - Pass (`scripts/tinkerboard-shell.txt`): `smp: Brought up 1 node, 4 CPUs` (also after
    `reboot -f`), `nproc` = 4, four parallel shell loops finish, plus everything M2 checked.
    The kernel log shows the A12 identity end to end: `CPU: PIPT / VIPT nonaliasing data
    cache, VIPT aliasing instruction cache`, `VFP support v0.3: implementor 41 architecture 3
    part 30 variant d rev 0` (the line RK3288 hardware prints) and the `armv7_cortex_a12`
    PMU driver.
- **M4: storage and a real image.** Done (2026-11-17).
  - Devices: dw_mmc for SD and eMMC; RK808 on I²C (Linux will not finish probing the SD
    regulators without it; it is also the system power controller for `poweroff`).
  - Boot: firmware-style from the image's `/boot` (`armbianEnv.txt` or `extlinux.conf`, zImage,
    dtb, initrd).
  - Pass: Armbian boots to `login:` from an unmodified SD image.
  - What it took:
    1. `arm-a32`: STM with the PC in the register list. Armbian's kernel is built with frame
       pointers (`UNWINDER_FRAME_POINTER`), so every function starts with `push {fp, ip, lr,
       pc}`; the zImage decompressor died on its first call.
    2. `dwmmc.rs`: one model per controller (base, interrupt, `CardKind::Sd` or `Emmc`):
       commands complete when written; IDMAC 32-bit descriptors (chained and ring) for block
       data; the FIFO at 0x200 for the transfers Linux does by PIO (< 16 bytes: the SD SCR);
       card detect in CDETECT; VERID 2.70a, HCON for 32-bit IDMAC. Empty slots (SDIO0's
       Wi-Fi, an absent eMMC) answer nothing and Linux gives up cleanly.
    3. `emmc.rs`: an eMMC 5.1 device (CMD1 OCR with access mode, CID/CSD, EXT_CSD by CMD8,
       SWITCH by CMD6, erase by CMD35/36/38), HS52 only so Linux does no tuning; byte
       addressing up to 2 GiB with the CSD capacity, sector addressing above. `--emmc IMAGE`.
    4. `i2c.rs`: the rk3x controller (TX, RX and REGISTER_TX modes, NAK on empty addresses)
       with an `I2cDevice` trait; `rk808.rs`: regulators that read back what Linux writes
       (U-Boot-like voltages at power-on), the RTC in the RK808's own calendar (November has
       31 days; Linux converts), started from the host clock, and the DEV_OFF bit.
    5. `gpio.rs`: input levels. With the GPIO banks as read-back stubs, the power button
       (`gpio-keys`, GPIO0_A5, active low) read as pressed and systemd-logind powered the
       board off right after boot; inputs now read high.
    6. `boot::boot_from_card`: Armbian's `boot-rockchip.cmd` (armbianEnv.txt; zImage,
       `dtb/rk3288-tinker-s.dtb` unless `fdtfile` says otherwise, uInitrd; root by UUID,
       `ubootpart` from the MBR id) on the SD card, else the eMMC. The serial console goes
       last on the command line so `/dev/console` is ttyS2 (no display yet); overlays from
       armbianEnv.txt (`i2c1 i2c4 spi2 spidev2 uart1 uart2` in this image) are not applied.
  - Pass (`scripts/tinkerboard-armbian.txt`): the Armbian banner, `multi-user.target`,
    `tinkerboard login:`, the first-run wizard (root password; user creation skipped with
    Ctrl-C, `serial \x03` in scripts), then from the root shell the board model,
    `mmcblk0p1` as `/`, the `rk808-rtc` clock, four CPUs and both SD regulators.
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

## State (2026-11-18)

Milestones: M0–M4 and M8 done; M7 done for the panel board's DSI panel but not HDMI; M9 half
done (the JIT, not threads); M5, M6 and M10 not started. The panel board profile runs the vendor's
firmware to its application with display, touch, Wi-Fi and audio (`docs/panel-board.md`).

| Area | State | Where |
|---|---|---|
| Cores | Four Cortex-A12 with Thumb-2, VFP and NEON (#18), on the A32/T32 JIT by default (#45; `--no-jit` or `SBC_JIT=0` for the interpreter). All four run on one host thread. | `arm-a32`, `machine.rs` |
| Interrupts, timers, SMP | GIC-400, the generic timer, the boot ROM SMP mailbox, TLB broadcast | `machine.rs` |
| Clocks, power | CRU (PLLs, the I2S fractional divider), GRF/SGRF, PMU; RK808 (Tinker Board) or ACT8846, SYR827/828 and HYM8563 (panel board) on I2C | `cru.rs`, `i2c.rs`, `peripherals/` |
| Storage | dw_mmc for SD (card detect on GPIO6_C6 for the panel board), eMMC and SDIO; boot from the card's `/boot` like the firmware | `dwmmc.rs`, `emmc.rs`, `boot.rs` |
| Display | VOP (both, two windows), IOMMU, DW MIPI DSI to the panel board's panel; VNC and `--screenshot` | `vop.rs`, `dsi.rs` |
| 2D and GPU | RGA; Mali-T760 MP4 for Panfrost + Mesa (kmscube, 25 of 25 GLES 2 scenes match softpipe) | `rga.rs`, `midgard/` |
| Touch | ILI2116 on I2C4 (panel board) | `peripherals/src/ili2116.rs` |
| Wi-Fi | BCM43455 on SDIO for brcmfmac: scan, open and WPA2 joins, DHCP and NAT on `sbc-net` (panel board) | `peripherals/src/bcm43455/` |
| Audio | I2S fed by the PL330 DMA controller at 0xffb20000; `--audio-out FILE.wav` or a FIFO | `i2s.rs`, `peripherals/src/pl330.rs` |

Not modelled yet, roughly in order of value:

1. **USB** (DWC2 OTG and host, EHCI/OHCI; M6). The panel board's pads and controls are an STM32
   on USB, and the Tinker Board needs it for a keyboard and sticks.
2. **Speed: one host thread per core** (M9). The vendor application runs at about 30 % of real
   time idle and 5 % while it plays a sample, too slow for live audio; recording with
   `--audio-out` works. This touches `arm-a32` and needs its owner.
3. **GMAC Ethernet** (M5). Armbian's first-login wizard waits about a minute for an address.
4. **HDMI** (M7). The Tinker Board has no screen output; the VOP is there, the DW HDMI is not.
5. The browser build and demo (M10).
6. Smaller: the second PL330 (0xff250000), backlight PWM, the SAR ADC, Bluetooth, the
   video codec IOMMUs, device tree overlays from `armbianEnv.txt`, LPAE kernels.

The panel board's own open items (the start-up "Wifi and storage systems failed" dialog, the
pads) are listed in `docs/panel-board.md`.

Times on an M-series host, one host thread (2026-11-18): `scripts/tinkerboard-shell.txt`
8 s, `scripts/tinkerboard-armbian.txt` 5 min 52 s, `scripts/panel-board-audio.txt` 2 min 13 s,
all on the JIT.

M4 state, kept for reference: Armbian 26.8.1 passed `scripts/tinkerboard-armbian.txt` in
22 min 45 s on the interpreter (51.7 G instructions summed over the cores, 24.3 s virtual,
about a minute of it the first-login wizard waiting for a network address). `poweroff` halts
instead of switching off: 6.18 registers the RK808 power-off as a POWER_OFF_PREPARE handler
only (`kernel_can_power_off()` is false), so the model's DEV_OFF path is unit-tested but not
reached by this kernel.

M3 state, kept for reference: The Debian 6.12.107 armmp kernel brings up four cores and
reaches the initramfs shell after about 4.3 G instructions summed over the cores (3.5 s
virtual, about 100 s on an M-series host; one host thread). Unmodelled pages Linux touches on
the way: timer 0xff810000, PMU SRAM 0xff720000, DMA 0xff250000/0xffb20000, GPIO banks
0xff750000-0xff7f0000, UART0/1/3/4, QoS 0xffaa0000-0xffaf0000.

The Cortex-A12 identity (`arm_a32::CORTEX_A12`) comes from the Cortex-A17 r1p1 TRM (Arm
publishes no A12 TRM; the A17 TRM still lists A12 values for FPSID and PMCR) and RK3288 boot
logs. Not verified for the A12 itself: the ID_*/CLIDR/CCSIDR values (A17 r1p1 values, consistent
with everything RK3288 Linux prints) and CTR's IminLine (the TRM's IMINLN input; 0x84448004 is
used). Deliberately not advertised: CP14 debug (ID_DFR0 debug fields 0; the kernel reports
`hw-breakpoint: debug architecture 0x0 unsupported`) and PMU event counters (PMCR.N = 0).
Advanced SIMD has been advertised in MVFR1 since NEON landed (#18).

## Core changes needed (arm-a32, owned by the Orange Pi One session)

Done: the Cortex-A12 identity as an `arm_a32::CpuModel` preset (CTR, CLIDR, CCSIDR, ID
registers, L2CTLR, PMCR IDCODE, FPSID, MVFR0/1) and the CP15 c15 diagnostic (op2 1) and internal
feature (op2 2) registers; Thumb-2; per-core state and WFE/SEV for SMP; STM with the PC in
the register list (M4, for frame-pointer kernels); NEON (#18); the A32/T32 JIT (#29, wired
into the RK3288 machine in #45). Still needed: running the cores on separate host threads
(M9), LPAE for `armmp-lpae` kernels, CP14 debug and PMU
event counters if a guest needs them (both are hidden in the ID registers today).

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
