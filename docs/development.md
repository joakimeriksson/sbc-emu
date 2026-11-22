# Development: tests and source layout

## Tests

```sh
cargo test --release --workspace
```

GitHub Actions runs the suite on every pull request and push to `main` ([.github/workflows/ci.yml](../.github/workflows/ci.yml)):
- macOS on Apple silicon: everything, including the JIT and the differential tests against the
  host CPU;
- Linux x86-64: the interpreter, as a portability check;
- the WebAssembly build.

- `arm-a64/tests/diff_host.rs`, `diff_neon.rs`, `diff_fp.rs`: **differential tests against the
  host CPU** (Apple silicon is AArch64): integer (incl. CRC32), FP and NEON snippets run natively
  and on the emulator from the same random states; registers, flags, NaN payloads and FPSR must
  match bit for bit. `diff_fp.rs` draws a random FPCR (rounding mode, FZ, DN) per case and
  operands from the hard classes (subnormals, overflow, ties, NaNs).
- `arm-a32/tests/neon_diff.rs`: the same for the 32-bit core's NEON: 407 A32 instructions (and
  their T32 encodings, from `arm-none-eabi-as`) against AArch64 equivalents run natively with
  FPCR.FZ/DN (ARMv7's Standard FPSCR value); `neon_ldst.rs` checks every VLD/VST encoding
  against a reference model.
- `bcm2711` unit tests: GPIO edges, BME280 compensation against the Linux driver's formula, MCP3008
  framing against `mcp320x`'s decode, GIC SGI/SPI routing between cores, DMA control-block chains,
  and `smp.rs`: four cores add to one counter with LDXR/STXR, one thread and a thread per core,
  interpreted and compiled (a plain store instead of the compare-and-swap loses ~70% of them).
- `arm-a64/tests/system.rs`: exceptions, EL changes, MMU faults, timer interrupts.
- `bcm2711/tests/watch.rs`: debugger watchpoints stop after the access, on the right core.
- `rk3588/tests/snapshot.rs`, `allwinner-h3/tests/snapshot.rs`: a restored machine goes on exactly as the saved one (bare metal, and
  about 30 points of a Linux boot when the guest is in `cache/`).
- `cli/src/gdb.rs`: each board's machine under the gdb stub (memory, registers, steps, breakpoints,
  watchpoints), a debugger that vanishes, a reset; `tools/gdb-check.py` does it on booted guests.
- `tools/wasm-smoke.mjs`, `tools/wasm-smoke-sd.mjs`: headless boots of the WebAssembly build.
- `tests/gpu/` + `scripts/gpu-gltest.txt`: OpenGL ES and Vulkan scenes rendered in the guest on the
  emulated V3D and on Mesa's software renderers, compared per pixel ([docs/gpu.md](gpu.md)).

## Source layout

```
emu-core/   Bus / Core / Trap, the snapshot serializer and a device tree reader/writer
arm-a64/    the ARMv8-A core: decode+execute, system registers, MMU, FP/NEON
arm-a32/    the ARMv7-A core for 32-bit boards: A32, Thumb-2, VFPv4, NEON, short-descriptor MMU
riscv-rv32/ the RV32 core reused from esp32sim (MIT, see its README and LICENSE), with M/S/U and Sv32 for the ESP32-S31
espressif/  esp32sim's ESP32-S31 SoC (esp32s31, esp-soc, esp-periph, its 32-bit emu-core), vendored from esp32sim d043ff1d (MIT)
mali400/      the Mali-400 MP2 GPU (Utgard): GP vertex shader and PLBU, PP fragment shader, tiles, MMUs, as Linux's and Mesa's lima drivers use it
peripherals/  device models shared between boards: GIC-400, DesignWare UART, the I²C device interface and I²C chips (RK808, ACT8846, SYR827/828 regulators, HYM8563 RTC), the 24 MHz timebase
midgard/      the Mali-T760 MP4 GPU (Midgard) of the RK3288: job manager, MMU, Midgard shader core, tiler, fragment pipeline, textures, as Linux's and Mesa's panfrost drivers use it; mali-replay for captured job chains
allwinner-h3/ the experimental Orange Pi One machine (H3): four cores, GIC-400, UART0, CCU, timers, SD/MMC, HDMI (DE2 mixer, TCON, DW-HDMI), USB (OHCI with keyboard and tablet, EHCI), Ethernet (EMAC, internal PHY), host PSCI, U-Boot-style DTB patching
riscv64/      an RV64GC hart (IMAFDC, Zba, Zbb; M/S/U; Sv39), its riscv-tests suite and a block JIT (to AArch64 natively, to WebAssembly in the browser)
starfive-jh7110/ the experimental Orange Pi RV machine (JH7110): four U74 harts, CLINT, PLIC, UART0, clock/reset generators, PMU, DW MMC (microSD), GMAC + YT8531C PHY, PLDA PCIe + VL805 xHCI, M.2 NVMe, SDIO Wi-Fi (BCM43456), Bluetooth on UART1, GPIO interrupts, AXI DMA + PWM DAC and HDMI I2S audio, DC8200 + Inno HDMI, I²C with the AXP15060 PMIC and EEPROM, the PKA crypto engine, TRNG, the boot ROM's SD path with the L2 LIM and DDR controller (U-Boot), OpenSBI direct boot
powervr/        Imagination PowerVR Rogue GPU (BXE-4-32): registers and the firmware interface for Linux's powervr driver
v3d/        the V3D 4.2 3D GPU (packets generated from Mesa's v3d_packet.xml by tools/gen_v3d_packets.py)
bcm2711/    the Pi 4 machine: GIC, PL011, mailbox, SDHCI, DWC2, PCIe + xHCI, USB devices, I²S, Bluetooth,
            GENET, device tree, boot, FAT
rk3588/     the experimental Orange Pi 5 machine (RK3588S): GICv3, UART2, timer
sbc-machine/ the board catalogue and the host-facing machine layer the boards share
sbc-net/    virtual network: DHCP/ARP/ICMP/DNS/NTP, user-mode NAT, 802.11 AP (from esp32sim)
cli/        the sbc-emu command
wasm/       C ABI for the browser (the Pi 4, the Tinker Board, the Orange Pi One and the Orange Pi RV); web/ is the pages and worker
scripts/    action scripts (boots, demos, GPU tests, Ubuntu)
```
