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
