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

## Orange Pi One (Allwinner H3), 2026-11-15

Started by an OpenCode session (bare-metal `arm-a32` and `allwinner-h3` slices), taken
over and finished to a Linux shell by Claude Code. `orangepi-one` is a distinct board
identity (`orangepi-pc` is not accepted as an alias).

**What it is.** One Cortex-A7 on the interpreter-only `arm-a32` core (A32, VFPv4-D32,
short-descriptor MMU with a TLB, physical and virtual architectural timers); the H3's
GIC-400, DesignWare UART0 with interrupts, CCU (PLL lock bits, APB2 from OSC24M) and timer
block. Every other address in the H3 device window (0x0100_0000-0x01ff_ffff) is a
read-back register file, so the drivers of the distribution DTB probe without effect; the
run summary lists the pages touched. Virtual time: 1 ns per instruction, the 24 MHz count
advances 3 ticks per 125 instructions; `run` reports idle deadlines so the shared serial
loop paces idle time against the wall clock.

**Boot path.** Direct kernel boot with the distribution's DTB, adjusted as U-Boot adjusts
it on the board: `/chosen` (bootargs, initrd), `/memory` (emulated DRAM size, 1 GiB by
default) and a `/psci` node (SMC conduit) answered by the host (`PSCI 1.0`: version,
features, system off/reset, affinity info; CPU_ON for secondaries is not offered yet).
Load addresses are U-Boot's sunxi defaults. Unlike the Pi 4 (D2) no reduced DT is
generated: the mainline H3 tree works unmodified because unmodelled devices read back.

**Fixture.** `scripts/orangepi-one-fixture.sh` downloads and verifies Alpine 3.24's
`linux-lts-6.18.63-r0` (armv7) and `alpine-minirootfs-3.24.4-armhf`, extracts the zImage
and `sun8i-h3-orangepi-one.dtb` unmodified and builds the initramfs (minirootfs + a
serial-shell `/init`).

| Artefact | SHA-256 |
|---|---|
| `zImage` (vmlinuz-lts) | `7bc559959b8eebc56c9417c92e5e4e50eb327ca43a2b78e40a691de3395f8a46` |
| `sun8i-h3-orangepi-one.dtb` | `42e7943844553dd4c9b515ce762b290481eb33fecea08a3ddbf72a451463033e` |
| `alpine-minirootfs-3.24.4-armhf.tar.gz` | `5d3c3debe55cbfca4f5b4252b0a3a7f75434bb6b6e3e1859fe917e02a4a326ed` |
| `linux-lts-6.18.63-r0.apk` | `676ce55f608d0d19e18af783cadc70b80fd0da32dd01a85df589c2668daa485b` |

**Evidence.** `scripts/orangepi-one-shell.txt` passes end to end: prompt, `uname`, DT model,
VFPv4 in `/proc/cpuinfo` and `awk` floating point, `sleep 1` and `/proc/interrupts`, tmpfs
write/read, `reboot -f` through PSCI to a second boot, `poweroff -f` through PSCI (two boots,
41 s host time). `cargo test --release --workspace`: 124 passed, 0 failed.
`tools/multi-board-smoke.py` on the Pi 4 fixtures: single and threaded runs pass.

**Bugs found on the way** (each has a regression test in `arm-a32/tests/linux_userland.rs`):
memory accesses with the MMU on were split into byte accesses with a table walk each
(device registers broke, so the console stayed silent); post-indexed loads/stores wrote the
base back when the access aborted (Linux's `__clear_user` retry then wrote one byte past
the page and `execve` failed with EFAULT); `MLS`/`UMAAL` and the miscellaneous space
(`SMC`, `QADD`, `SMLAxy`) fell through to data processing and executed as the wrong
instruction; `CPS` decoded its A/I/F bits from the wrong positions.

**Not yet.** SD/MMC, EMAC, USB, display, SMP (PSCI CPU_ON plus a second `arm-a32` core),
Thumb-2 and NEON (both needed for Debian/Armbian armhf userlands and the RK3288 plan),
a JIT, snapshots and the browser build. The RK3288 / panel board work builds on `arm-a32`;
`arm-a32` needs a per-board CPU identity (MIDR, cache and feature ID registers) before a
second board uses it.
