# NVIDIA Jetson Orin Nano 4GB

`--board jetson-orin-nano-4gb`, experimental.

The Tegra234 target boots an arm64 Linux kernel directly using a generated
device tree. The bring-up configuration has 4 GiB of RAM and six Cortex-A78AE cores across two
clusters. It provides host PSCI, a GICv3 distributor and per-core redistributors,
ARM architectural timers, and a serial console on the SBSA UART at
`0x031d0000`. The CLI supports the serial runner, scripts, and GDB.

## Boot Alpine

```sh
cargo build --release -p sbc-emu
sh scripts/jetson-nano-fixture.sh cache/jetson-orin-nano orin
D=cache/jetson-orin-nano
target/release/sbc-emu run --board jetson-orin-nano-4gb \
  --kernel "$D/vmlinuz" --initrd "$D/initramfs.cpio.gz" \
  --script scripts/jetson-orin-nano-shell.txt --timeout 120
```

The fixture uses the checksummed Alpine packages shared with the Tegra210
fixture. For interactive use, omit `--script` and `--timeout`. Use
`--ram-mb 768` for a smaller interactive guest, `--smp 1` for single-core bring-up,
or `--no-jit` for the interpreter. The scripted check expects six cores and 4 GiB.
It checks an architectural timer interrupt and a GIC IPI on every core, then
offlines and onlines CPU 5.

## Rover UART

The bring-up device tree exposes Tegra UART A at `0x03100000` as `/dev/ttyS1`
through the Alpine fixture's `8250_tegra` driver. Open it at 115200 baud in raw mode.
The port is independent of the SBSA console.

Use `--rover-uart loopback` to test the port without a peer, or pass the numeric
address of the ESP32 simulator's UART TCP listener, for example
`--rover-uart 127.0.0.1:5500` with esp32sim's `--uart-tcp 127.0.0.1:5500`.
The link carries raw bytes. Start the listener before the board and keep it
running. A disconnected peer ends the run with an error.

```sh
scripts/quickstart.sh jetson-orin-nano-4gb --rover-uart loopback \
  --script scripts/jetson-orin-nano-uart.txt --timeout 60
```

The loopback script compares returned bytes for a two-byte write and a burst
larger than the RX FIFO.

### High-speed Tegra UART driver

Use `--uart-driver tegra` with a kernel built with `CONFIG_SERIAL_TEGRA`.
It binds the same MMIO port to Linux's Tegra PIO driver at `/dev/ttyTHS1`,
the device expected by the Waveshare ROS driver.

By default, the bring-up device tree provides a 115200-baud clock and a simple
reset register at `0x70000000`. The driver can warn when it tries its initial
9600-baud setting. Configure the port at 115200 baud before sending data.

The Debian kernel pinned by the Orange Pi 5 fixture has the driver built in.
Use that kernel with the Orin initramfs to run the same byte comparison through
`ttyTHS1`:

```sh
sh scripts/orangepi5-fixture.sh cache/orin-debian-kernel
sh scripts/jetson-nano-fixture.sh cache/jetson-orin-nano orin
target/release/sbc-emu run --board jetson-orin-nano-4gb \
  --uart-driver tegra --rover-uart loopback \
  --kernel cache/orin-debian-kernel/Image \
  --initrd cache/jetson-orin-nano/initramfs.cpio.gz \
  --script scripts/jetson-orin-nano-uart.txt --timeout 60
```

## HSP and BPMP

Pass `--bpmp on` to expose the HSP TOP0 doorbells and BPMP shared-memory
queues in SYSRAM. With `--uart-driver tegra`, the device tree then points UART A
to BPMP for its programmable clock and reset. The guest kernel needs the
Tegra HSP mailbox, SRAM, and BPMP drivers for this configuration.

The firmware model follows the [Linux BPMP transport](https://github.com/torvalds/linux/blob/master/drivers/firmware/tegra/bpmp-tegra186.c)
and [BPMP ABI](https://github.com/torvalds/linux/blob/master/include/soc/tegra/bpmp-abi.h).
It implements IVC reset and request/reply handling, firmware identification, and
the clock, reset, and power-domain requests for the modeled devices. Run
`cargo test -p tegra234` to exercise atomic and threaded requests over the SoC
bus, including UART clock and reset changes and GPU power gating. The Debian
fixture can boot with the generated nodes but lacks the HSP mailbox driver.

### NVIDIA kernel smoke test

Use NVIDIA's [Jetson Linux R36.4.4](https://developer.nvidia.com/embedded/jetson-linux-r3644)
arm64 `nvidia-l4t-kernel` package version `5.15.148-tegra-36.4.4-20250616085344`.
Its SHA-256 is `f26ccf85c64f9d4c36001b0d04df8c8bdd370aa173ebdcacb4f4fc3995420500`.
Extract `boot/Image` to `cache/jetson-r3644/Image`, then use the Alpine initramfs
from the Orin fixture.

```sh
target/release/sbc-emu run --board jetson-orin-nano-4gb \
  --bpmp on --uart-driver tegra --rover-uart loopback \
  --kernel cache/jetson-r3644/Image \
  --initrd cache/jetson-orin-nano/initramfs.cpio.gz \
  --script scripts/jetson-orin-nano-uart.txt --timeout 60
```

The kernel reports `tegra-bpmp bpmp: firmware: sbc-emu-tegra234` and registers
`ttyTHS1`. The script verifies the short and burst byte comparisons through
that device at 115200 baud.

## Ampere GPU bring-up

Like `tegra210/src/gpu/` for the original Jetson Nano, `tegra234/src/gpu/`
contains the GPU's register model. The first slice exposes the GA10B identity
and PTIMER at BAR0 (`0x17000000`). A guest-CPU test reads these registers
through Tegra234's MMIO bus. When BPMP is enabled, the GPU power domain and
reset control access to those registers. The generated device tree does not
yet expose a GPU node for driver binding.

Use the Orin `nvgpu` module from [Jetson Linux R36.4.4](https://developer.nvidia.com/embedded/jetson-linux-r3644)
as the baseline for driver bring-up. The remaining milestones are:

1. Add the GA10B device-tree node using the R36.4.4 P3767-0004 device tree and
   stage `nvgpu` with its `host1x`, `mc-utils`, and `nvmap` dependencies. Use the
   vendor probe to identify the registers and firmware handshakes it needs.
2. Implement BAR1 and GMMU translation, GPU interrupts, firmware handshakes,
   and a channel/runlist path. Verify a command submission and a memory copy
   with data and completion checks.
3. Add Ampere shader and compute execution against a known-answer workload.
   Validate the userspace stack with a CUDA kernel on the emulated GPU.

[NVIDIA GA10B device resources](https://developer.nvidia.com/docs/drive/drive-os/6.0.8/public/drive-os-linux-sdk/common/topics/util_setup/nvgpu_device_tree.html) and the
[GA10B HAL setup](https://github.com/OE4T/linux-nvgpu/blob/patches-rel-36/drivers/gpu/nvgpu/hal/init/hal_ga10b.c)
describe the MMIO windows, firmware dependencies, and shared NVIDIA blocks.

The CPU affinities follow an
[Orin Nano Linux boot log](https://forums.developer.nvidia.com/t/orin-nano/385080).
The other hardware addresses and interrupt bindings follow the
[Linux Tegra234 device tree](https://github.com/torvalds/linux/blob/master/arch/arm64/boot/dts/nvidia/tegra234.dtsi).
