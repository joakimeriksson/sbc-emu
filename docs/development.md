# Development: tests and source layout

## Tests

```sh
cargo test --release --workspace
```

GitHub Actions runs the suite on every pull request and push to `main` ([.github/workflows/ci.yml](../.github/workflows/ci.yml)):
- macOS on Apple silicon: everything, including the JIT and the differential tests against the
  host CPU;
- Linux x86-64: all tests, with the x86-64 JIT backends of all four cores (`SBC_JIT=1` for the
  64-bit Arm core's tests too; `arm-a64/tests/jit_diff.rs`, `arm-a32/tests/jit_diff.rs` and
  `jit_flags.rs`, the RV64 core's random programs (`riscv64/src/jit_check.rs`) and
  `riscv-rv32/tests/blocks_differential.rs` compare them with the interpreters; the encoders have
  tests against an assembler);
- the WebAssembly build, and the Jetson Nano's shader JIT in it against the interpreter.

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
- `rk3588/tests/snapshot.rs`, `allwinner-h3/tests/snapshot.rs`,
  `rockchip-rk3288/tests/snapshot.rs`, `starfive-jh7110/tests/snapshot.rs`: a restored machine goes on exactly as the saved one (bare metal, and
  about 30 points of a Linux boot when the guest is in `cache/`).
- `cli/src/gdb.rs`: each board's machine under the gdb stub (memory, registers, steps, breakpoints,
  watchpoints), a debugger that vanishes, a reset; `tools/gdb-check.py` does it on booted guests.
- `tools/wasm-smoke*.mjs`: headless boots of the WebAssembly build, one per board.
- `tests/gpu/` + `scripts/gpu-gltest.txt`: OpenGL ES and Vulkan scenes rendered in the guest on the
  emulated V3D and on Mesa's software renderers, compared per pixel ([docs/gpu.md](gpu.md)); the
  same scenes on the other GPUs (each GPU's page names its script).

## Source layout

How the pieces fit together: [architecture.md](architecture.md).

```
emu-core/       Bus / Core / Trap, the snapshot serializer, disks and SD cards, ext4, GPT, a device tree reader/writer
arm-a64/        the ARMv8-A core: decode+execute, system registers, MMU, FP/NEON; JIT to AArch64, x86-64 and WebAssembly
arm-a32/        the ARMv7-A core for 32-bit boards and programs: A32, Thumb-2, VFPv4, NEON, short-descriptor MMU; JIT to AArch64, x86-64 and WebAssembly
riscv64/        an RV64GC hart (IMAFDC, Zba, Zbb; M/S/U; Sv39), its riscv-tests suite and a block JIT (to AArch64 and x86-64 natively, to WebAssembly in the browser)
riscv-rv32/     the RV32 core reused from esp32sim (MIT, see its README and LICENSE), with M/S/U and Sv32 for the ESP32-S31; JIT to AArch64, x86-64 and WebAssembly
espressif/      esp32sim's ESP32-S31 SoC (esp32s31, esp-soc, esp-periph, its 32-bit emu-core with the JIT encoders), vendored from esp32sim d043ff1d (MIT)
peripherals/    device models shared between boards: GIC-400, DesignWare UART and HDMI TX, PL330 DMA, xHCI, SDIO, BCM43455 Wi-Fi, the I²C device interface and I²C chips (RK808, ACT8846, SYR827/828, HYM8563), the 24 MHz timebase
sbc-net/        virtual network: DHCP/ARP/ICMP/DNS/NTP, user-mode NAT, 802.11 AP (from esp32sim)
bcm2711/        the Pi 4 machine: GIC, PL011, mailbox, SDHCI, DWC2, PCIe + xHCI, USB devices, I²S, Bluetooth, Wi-Fi,
                GENET, HDMI, device tree, boot, FAT
v3d/            the V3D 4.2 3D GPU (packets generated from Mesa's v3d_packet.xml by tools/gen_v3d_packets.py)
vc4-3d/         the VideoCore IV 3D GPU (V3D 2.1) of the Pi 3: control lists, binner, rasterizer, tile buffer, QPU, TMU, VPM; vc4-replay for captured jobs
rk3588/         the Orange Pi 5 machine (RK3588S): GICv3, UART2, timer, SD (DW MMC), GMAC, USB 3.0 xHCI, PCIe + NVMe, VOP2 + HDMI QP, I²S + ES8388 audio, SCMI, and the Mali-G610 GPU (gpu*.rs)
allwinner-h3/   the Orange Pi One machine (H3): four cores, GIC-400, UART0, CCU, timers, SD/MMC, HDMI (DE2 mixer, TCON, DW-HDMI), USB (OHCI with keyboard and tablet, EHCI), Ethernet (EMAC, internal PHY), host PSCI, U-Boot-style DTB patching
mali400/        the Mali-400 MP2 GPU (Utgard): GP vertex shader and PLBU, PP fragment shader, tiles, MMUs, as Linux's and Mesa's lima drivers use it
rockchip-rk3288/ the Tinker Board S machine (RK3288): four Cortex-A12, GIC-400, CRU/GRF/PMU, boot ROM SMP mailbox, DW MMC (SD, eMMC), I²C with the RK808, GPIO, VOP + DW HDMI, MIPI DSI, RGA, I²S + PL330
midgard/        the Mali-T760 MP4 GPU (Midgard) of the RK3288: job manager, MMU, Midgard shader core, tiler, fragment pipeline, textures, as Linux's and Mesa's panfrost drivers use it; mali-replay for captured job chains
starfive-jh7110/ the Orange Pi RV machine (JH7110): four U74 harts, CLINT, PLIC, UART0, clock/reset generators, PMU, DW MMC (microSD), GMAC + YT8531C PHY, PLDA PCIe + VL805 xHCI, M.2 NVMe, SDIO Wi-Fi (BCM43456), Bluetooth on UART1, GPIO interrupts, AXI DMA + PWM DAC and HDMI I2S audio, DC8200 + Inno HDMI, I²C with the AXP15060 PMIC and EEPROM, the PKA crypto engine, TRNG, the boot ROM's SD path with the L2 LIM and DDR controller (U-Boot), OpenSBI direct boot
powervr/        Imagination PowerVR Rogue GPU (BXE-4-32): registers and the firmware interface for Linux's powervr driver
tegra210/       the Jetson Nano machine (Tegra X1): four Cortex-A57, GIC-400, timers, CAR, PMC, SDMMC, I²C + MAX77620, GPIO, SMMU, PCIe + RTL8111H, XUSB, host1x, display controllers + SOR/HDMI, the GM20B's host side
maxwell/        the Maxwell GPU engine: 3D with its macro processor, 2D, compute, inline to memory
maxwell-shader/ the Maxwell shader instruction set: decoder, disassembler, interpreter, JIT to WebAssembly
maxwell-tex/    Maxwell texture and sampler descriptors, block-linear layouts, filtering, compressed formats
sbc-machine/    the board catalogue and the host-facing machine layer the boards share
cli/            the sbc-emu command
wasm/           C ABI for the browser (all seven boards)
web/            the browser pages and the worker
demos/          the Jetson Nano's GPU demos (Rust, OpenGL ES 3.2)
scripts/        fixture scripts (pinned guest downloads) and action scripts (boots, checks, demos, GPU tests)
tests/gpu/      the GPU test programs run inside the guests
tools/          the site and browser builds, headless browser-build checks, GPU test servers, generators
docs/           this documentation; docs/history/ has the plans and development records
```
