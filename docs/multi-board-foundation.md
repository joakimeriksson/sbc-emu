# Multi-board foundation: first implementation slice

Implemented 2026-11-15 on branch `codex/multi-board-foundation`, based on `dafb3c2`.
Worktree: `/private/tmp/sbc-emu-multi-board`.

## Scope and deliberate transition

- `sbc-machine` holds known board IDs and a small enum-based host interface.
- `pi4b` remains the default. `pi3b` and `orangepi5` are recognized but fail explicitly
  as unimplemented before image files, sockets or guest resources are opened.
- Both `--board ID` and `--board=ID` work. Missing, unknown and duplicate selectors
  fail instead of accidentally selecting Pi 4.
- `cli/src/pi4.rs` contains the existing native Pi setup, boot/reset policy, scripts
  and optional facilities. The move is mechanical except for common run, idle and
  serial operations now going through `sbc-machine::Machine`.
- The facade is a **borrowed enum view** in this first slice. The Pi adapter still
  owns its concrete machine. This narrows the original owned-facade proposal so
  optional Pi operations do not require a new whole-Soc accessor or widespread
  forwarding methods. Revisit ownership when a second runner exists.
- `Stop` is temporarily re-exported from BCM2711, preserving all existing payloads.
  Move its definition to a neutral location when Rockchip needs to produce stops.

This completes board selection and the first common execution/serial boundary, not
the entire facade migration. Construction/reset, debug, screenshot, snapshot and
other script facilities remain in the Pi adapter. Their existing behavior is retained.
The CPU, SoC, scheduler, threaded runner, snapshot format and browser wiring are
unchanged. Orange Pi hardware is not implemented.

The user's instruction to proceed authorized this implementation slice. Claude's
earlier review supported the overall direction; it has not reviewed this implementation
or the borrowed-view adjustment, and no joint approval is claimed.

## Validation

- `cargo test --release --workspace`: **70 passed, 0 failed, 0 ignored**. The first
  sandboxed attempt denied socket binds in six existing NAT tests. Re-running with
  localhost socket access passed the entire workspace.
- New tests cover early board rejection, unchanged implicit/explicit Pi selection,
  malformed selectors, serial FIFO backpressure/order and notification, output
  draining, and identical direct/facade execution and saved state after idle advancement.
- `python3 tools/multi-board-smoke.py --kernel PATH/TO/Image --initrd PATH/TO/initramfs.cpio`:
  both single-threaded and threaded four-core configurations passed Linux boot to
  BusyBox, serial round trip, monitor reset, second boot and another serial round trip.
  The single-threaded run uses the implicit default board; threaded uses explicit Pi 4.
  The harness checks script milestones because CLI timeout alone returns success.
- `RUSTC="$(rustup which rustc)" "$(rustup which cargo)" check -p sbc-machine -p sbc-emu-wasm --target wasm32-unknown-unknown`:
  passed. Homebrew's compiler lacked wasm std; the installed rustup compiler has it.
- `git diff --check`: passed. Existing compiler warnings remain.

Direct-boot fixtures used (read-only from the active checkout's cache):

| Fixture | SHA-256 |
|---|---|
| `cache/fw/Image` | `c7ec21b9adbf9fb995d130121f35e642bb00e17a25ffe97b8253ae3bb7521c3b` |
| `cache/initramfs.cpio` | `274ad81c2adfabee0fdb7c3ccc4bac08ae3948a34ca6ed14579ec6af9b69a57d` |

Smoke commands, fixture hashes and logs are in this worktree's
`out/multi-board-smoke/report.json`, `single.log` and `multi.log`.
Workspace test output is `/private/tmp/sbc-multi-tests-unrestricted.log`.
These logs are local artifacts, not checked-in fixtures.

Full Raspberry Pi OS/Ubuntu image boots, browser runtime/GPU benchmarks and shipped
browser snapshot restores were not rerun. This change does not alter their hardware,
CPU, memory, DMA or snapshot implementations. Recheck affected paths if integration
with concurrent Pi work changes those boundaries. No performance claim is made.

## Handoff to Claude Code

Review the new facade and board parser, then compare `cli/src/pi4.rs` against the old
runner in `dafb3c2:cli/src/main.rs`. It should contain only the documented boundary
changes. Keep active VNC and other Pi improvements: they were uncommitted in the
original checkout and deliberately excluded from this branch.

Before integration, commit the concurrent Pi work on its own branch and merge/rebase
carefully: changes to the old main runner now belong in `cli/src/pi4.rs`. In particular,
preserve new VNC modules/imports, setup and usage text. Do not overwrite the active
checkout or take this branch's older Pi adapter wholesale over those changes.

The next hardware slice is a separate Rockchip machine with UART/GICv3 and direct
kernel/initramfs boot. Coordinate per-core CPU identity and system-register hooks
before editing `arm-a64`. No empty Rockchip crate has been introduced here.

## Orange Pi One adaptation (2026-11-15)

Branch `orangepi-one` retains the Rockchip board identity and adds the distinct
`orangepi-one` selector for the 1 GiB Allwinner H3 board. It is listed as not
implemented and rejected before image resources are opened; `orangepi-pc` is not
silently accepted as an alias. No Orange Pi hardware support is claimed yet.

The H3 slice cannot reuse the AArch64 core or the Rockchip GICv3 work. The new
interpreter-backed `arm-a32` crate implements an initial A32 subset: data processing,
MOVW/MOVT, multiply, word/byte transfers, branches, basic exception entry, MRS and a
small CP15 identity surface. The new `allwinner-h3` crate connects one core to based
RAM and polled, four-byte-spaced 16550 UART0 registers. Its integration test executes
real instructions at `0x4000_0000` which write `OK` to UART0 at `0x01c2_8000`.

A second slice adds a one-CPU GIC-400 distributor/CPU interface with PPI/SPI priorities,
level/edge inputs, acknowledge and EOI, plus the physical architectural timer's
`CNTFRQ`, `CNTKCTL`, `CNTPCT`, `CNTP_TVAL/CVAL` and `CNTP_CTL` CP15 registers. The timer
drives PPI 30 through the GIC. An integration test executes timer setup instructions,
sleeps in WFI, enters the IRQ vector and acknowledges/ends the interrupt. Counter time
currently advances one virtual tick per machine step, not at wall-clock 24 MHz.

The Linux-oriented CPU slice adds MSR, LDM/STM in all four addressing modes,
halfword/signed transfers, CLZ, bitfield extract, reverse/extend operations, BLX,
long multiply, ARM divide, barrier/prefetch hints and the early CP15 identity/setup
surface. Stock `multi_v7_defconfig` uses an A32 kernel with short descriptors rather
than a Thumb-2 kernel or LPAE. The MMU walker implements sections, large/small pages,
TTBR0/TTBR1/TTBCR selection, domains, AP/APX, XN/PXN and fault address/status registers.
Tests cover execution through mappings, page-crossing accesses and translation,
permission and instruction-fetch faults.

This remains a bare-metal slice. Thumb, VFP/NEON, LPAE, exclusive synchronization,
`zImage`/initramfs boot and a device tree are not implemented; the CLI still rejects
`orangepi-one` as unimplemented. Remaining kernel instructions/direct boot, Sunxi MMC,
native EMAC and four-core Sun8i CPUCFG/PRCM startup follow as separately tested
increments. Threaded execution, snapshots, browser support, stock-image boot, USB,
HDMI and Mali remain unsupported until their own evidence gates pass.

After serial Linux boots, a reserved-RAM `simple-framebuffer` with fbcon can provide a
visible generic firmware framebuffer; that is not native H3 HDMI. Native H3 DRM/KMS
requires the display clocks/resets, DE2 mixer0 at `0x0110_0000`, TCON0 at `0x01c0_c000`,
DW-HDMI at `0x01ee_0000` and its PHY at `0x01ef_0000`, plus virtual-monitor EDID/HPD,
primary-plane scanout and vblank/page-flip behavior.

Validation on `orangepi-one`: `cargo test --release --workspace` passed all 92 tests,
including 17 A32/MMU and five H3 integration tests; targeted rustfmt checks and
`git diff --check` passed; and the existing smoke harness passed Pi boot, serial input
and reset in both single-threaded and threaded modes.
The fixture hashes are the same as above. A whole-workspace rustfmt check still reports
pre-existing formatting drift in unrelated files, so no broad reformat was made.
