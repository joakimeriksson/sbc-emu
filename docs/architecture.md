# How the boards share code

sbc-emu is one Cargo workspace. Every board is a machine built from the same kinds of parts: a
CPU core, a model of its SoC with the devices on it, and the host side that runs it on a
terminal, a VNC viewer or a web page. Nothing is borrowed from QEMU ([decisions.md](decisions.md),
D1).

```
  cli (sbc-emu) -- sbc-machine: the board catalogue,        wasm + web/ (the browser)
        |          a common view of the machines                     |
        +------------------------------+-------------------------------+
                                       |
  bcm2711  rk3588  allwinner-h3  rockchip-rk3288  starfive-jh7110  tegra210  espressif/esp32s31
     |        |          |              |                 |            |              |
  arm-a64  arm-a64    arm-a32        arm-a32           riscv64      arm-a64      riscv-rv32
  v3d      (G610)     mali400        midgard           powervr      maxwell*
                \__________ peripherals, sbc-net, emu-core ____________/
```

## The layers

**`emu-core`** is the contract between a core and the machine around it: `Bus` (memory and
device accesses by physical address), `Core` (what a machine drives) and `Trap` (why a step
stopped), plus the `Stop` reasons a machine reports to the host (budget used, idle until a
given time, power-off, reset, a breakpoint or watchpoint). It also has what every board needs
and no chip knows about: disk images with copy-on-write overlays, the SD card protocol, GPT,
a read-only ext4 reader for booting from `/boot`, a device tree reader and writer, the USB
device side, debugger state, and the snapshot serializer (`snap`, see
[snapshots.md](snapshots.md)).

**The CPU cores**, each an interpreter with a block JIT ([jit.md](jit.md)):

| Crate | Architecture | Boards |
|---|---|---|
| `arm-a64` | ARMv8-A, AArch64 (EL0–EL3, MMU, FP/NEON, generic timer) | Raspberry Pi 4, Raspberry Pi 3, Orange Pi 5, Jetson Nano |
| `arm-a32` | ARMv7-A (A32, Thumb-2, VFPv4, NEON, short-descriptor MMU); also 32-bit programs under a 64-bit kernel | Orange Pi One, Tinker Board; 32-bit processes on the Arm64 boards |
| `riscv64` | RV64GC (Zba, Zbb; M/S/U; Sv39) | Orange Pi RV |
| `riscv-rv32` | RV32IMAC with M/S/U and Sv32 (from esp32sim, MIT) | ESP32-S31 |

A machine creates its cores with the identity of the real ones (MIDR, ID registers, cache
geometry), so the guest sees the cores it expects.

**The SoC crates** (`bcm2711`, `rk3588`, `allwinner-h3`, `rockchip-rk3288`, `starfive-jh7110`,
`tegra210`, `espressif/esp32s31`) each own a `Machine`: the cores, RAM, the SoC's devices at
their addresses, the virtual clock that orders everything, and the board's boot: loading a
kernel and device tree, or reading them from the card the way the board's firmware would. A
machine's `run(budget)` runs the cores for a slice and returns a `Stop`. `bcm2837` (Raspberry Pi
3) is a machine of its own that takes the blocks it shares with the BCM2711 from `bcm2711`.

**The GPU crates** are separate so that their size stays out of the SoC crates: `v3d`
(Raspberry Pi 4), `mali400` (Orange Pi One), `midgard` (Mali-T760, Tinker Board), `powervr`
(Orange Pi RV), `maxwell`, `maxwell-shader` and `maxwell-tex` (Jetson Nano). The Orange Pi 5's
Mali-G610 is in `rk3588`.

**`peripherals`** holds device models more than one SoC uses: the GIC-400, the DesignWare UART,
the DesignWare HDMI transmitter and its EDID, the PL330 DMA controller, the xHCI host
controller core, SDIO, the I²C device interface and I²C chips (PMICs, regulators, an RTC), a
Broadcom Wi-Fi chip, and the 24 MHz timebase. Models that started in one board (the Pi 4's
xHCI, the Orange Pi RV's NVMe) are reused by others as they are needed.

**`sbc-net`** is the virtual network behind every Ethernet and Wi-Fi model: a 10.0.2.0/24
subnet with DHCP, DNS, ARP, ICMP and NTP, user-mode NAT to the host's network, and an 802.11
access point (from esp32sim).

**`sbc-machine`** is the command line's view of the boards (the browser build drives the
machines directly): the catalogue (`Board`, the IDs and the
descriptions `sbc-emu boards list` prints) and `Machine`, an enum that borrows any board's
machine for what the host does the same way on all of them (run, idle, serial input and
output).

**`cli`** is the `sbc-emu` command. `main.rs` parses the board and hands over to that board's
runner (`pi4.rs`, `orangepi.rs`, `orangepi_one.rs`, `tinkerboard.rs`, `orangepi_rv.rs`,
`jetson_nano.rs`, `esp32s31.rs`). The runners other than the Pi 4's share `serial_board.rs`:
option checks, `--script`, the terminal with its Ctrl-A keys, pacing guest time to the wall
clock, reset and power-off, `--vnc` and `--screenshot`. The GDB stub (`gdb.rs`), the VNC
server (`vnc.rs`) and the script commands are the same code on every board.

**`wasm`** is the browser build: a C ABI (no wasm-bindgen) over the same machines, one board
per module instance, driven from a Web Worker (`web/worker.js`). The JITs have WebAssembly
backends that compile hot code into small modules the page instantiates.

## Time and threads

Each machine has one virtual clock. By default its cores take turns on one host thread in
fixed slices, and devices act at virtual times, so the same run gives the same result every
time. When all cores wait for an interrupt, the clock moves on to the next device event, never
ahead of the wall clock. The Raspberry Pi 4, the Orange Pi 5 and the Jetson Nano can
instead run a host thread per core (`--threads multi`): faster, and no longer repeatable.

## Adding a board

1. A SoC crate with a `Machine` on an existing core, its devices (from `peripherals` where
   they exist) and its boot.
2. An entry in `sbc-machine`'s `Board` and `Machine`.
3. A runner in `cli` on `serial_board.rs`, a fixture script that fetches a pinned guest, a
   `scripts/BOARD-shell.txt` check, and a case in `scripts/quickstart.sh`.
4. Snapshots: `emu_core::snap` for every device, and a test like `rk3588/tests/snapshot.rs`.
5. The browser: a boot function in `wasm/`, its boot branch in `web/worker.js` and an entry in
   `web/board.html`.

The development record of the first multi-board step is in
[history/multi-board-foundation.md](history/multi-board-foundation.md).
