# Multi-board preparation contract

> **Historical record, archived 2026-11-19.** The proposal and review below describe
> the repository on 2026-11-15, before Orange Pi 5 implementation. Their pending
> approvals, ownership notes and integration gates are historical, not current
> working instructions. Follow [AGENTS.md](../../AGENTS.md) for current workflow.
>
> The foundation landed in [PR #1](https://github.com/joakimeriksson/sbc-emu/pull/1);
> Orange Pi boot and GPU support have since advanced through
> [PR #65](https://github.com/joakimeriksson/sbc-emu/pull/65). See
> [the foundation implementation record](multi-board-foundation.md),
> [Orange Pi 5 support](../orangepi5.md) and [GPU capabilities](../orangepi5-gpu.md).
> The original proposal and both agents' review text are preserved below;
> this archive does not assert a new joint approval. For how the boards share code
> today, see [architecture.md](../architecture.md).

Date: 2026-11-15
Status: design preparation only; no executable API or Orange Pi support added.
Parent: [minimal refactoring plan](multi-board-refactoring-plan.md).

## Safe parallel work boundary

Pi development continues in the existing checkout. This preparation owns only
`docs/multi-board-refactoring-plan.md` and this file. Reviewers should append their
responses rather than replacing the other agent's review.

At inspection, HEAD was `bc40b1f`; local CPU edits existed in `arm-a64/src/jit.rs`,
`mmu.rs` and `sysreg.rs`, plus `scripts/ubuntu-bench.txt`. Claude also reserved
`bcm2711/src/mt.rs` for threaded-runner performance work. This is a point-in-time
inventory, not permission to edit any other file.

For executable preparation, use a separate worktree and branch from an agreed
committed baseline. It will not include uncommitted Pi fixes: record that limitation
and integrate those fixes after they land. Use that worktree's own target directory;
do not rebuild shared browser assets or overwrite caches, snapshots or boot fixtures
in the active Pi checkout. Coordinate heavy boot/performance runs so they do not
invalidate concurrent Pi measurements.

No new workspace members, dependencies, lockfile changes, CPU hooks or facade wiring
are needed for the documentation preparation. Avoid empty crates with no consumer.

## Execution contract to preserve

Source observations: `bcm2711/src/machine.rs::Machine::run`, `Stop`, and
`cli/src/main.rs` script/run loop. Recheck them when implementation starts.

| Property | Current Pi behavior | Initial Orange Pi contract |
|---|---|---|
| Deterministic run | Budget bounds aggregate retired instructions and also bounds execution attempts to stop fault loops | Same budget meaning; one core initially |
| Deterministic time | Virtual time follows CPU/device scheduling; multicore time is not simply aggregate retired instructions | Board-owned virtual clock, independent of host pacing |
| Threaded run | Eligible `multi` calls dispatch to a 10 ms wall-clock slice; the instruction argument is not a strict threaded limit | Unsupported; reject request |
| Debug fallback | Breakpoints, watchpoints or relevant CPU debug state can make Pi use its deterministic path | Preserve Pi behavior; expose effective mode if reporting it |
| Idle | `Idle(until)` gives an absolute virtual deadline in ns; host code decides pacing and advances idle time through the existing machine API | Same units and interpretation |
| Budget stop | Yield to caller, not shutdown or proof of progress | Same |
| Other stops | Preserve reset, power-off, unimplemented instruction, HLT/BRK, breakpoint/watchpoint and exception-storm payloads | Preserve applicable diagnostics; no silent success conversion |

The facade must distinguish requested and effective execution modes. Do not promise
determinism for threaded mode or relabel wall-clock slices as instruction budgets.
Keep the current Pi run dispatch unchanged in the first adapter. A future API can
express these modes more strongly once both implementations exist.

## Minimal host operations

These are responsibilities, not a frozen Rust trait or signature list:

- Board ID and supported operations: Pi 4 remains the default; an unavailable Orange
  Pi implementation produces a clear error before a guest starts.
- Construction and boot: board adapter owns wiring, kernel/initramfs or SD boot and
  reset policy. No common firmware assumption.
- Run: accepts existing budget configuration, returns a common stop reason and
  provides virtual time/instruction count for reporting.
- Idle advancement: preserve the existing machine's deadline handling and wakeups.
- Serial: enqueue accepted input with backpressure; drain output without losing bytes
  or leaving interrupt updates to frontend field manipulation.
- Optional operations: keyboard typing, screenshots and snapshot save/restore return
  explicit unsupported results if absent. Keep Pi-specific implementations in adapters.

An enum dispatch occurs at these host boundaries, not per instruction, RAM access or
device transaction. Avoid moving Pi scheduling merely to implement forwarding.

## Scripts that must survive facade migration

| Operation | Required behavior |
|---|---|
| `serial` | Preserve escape conversion, queueing and input backpressure |
| `wait-serial` | Match accumulated output; preserve clearing the match buffer after success |
| `wait-idle` | Preserve the current wall-clock time since the last busy run; this is not a deterministic guest-time assertion |
| `sleep` | Preserve current host-time wait semantics |
| `type` | Pi keyboard path remains; fail explicitly on boards without keyboard support |
| `shot` / pixel waits | Pi rendering remains; unsupported display must fail rather than wait forever |
| `save` | Existing Pi snapshot behavior remains; Orange Pi reports unsupported until implemented |

Keep other monitor, VNC, GDB and device-specific script commands in Pi adapters for
the first slice. Inventory actual command spellings and arguments before moving code.
Determinism validation must use a fixed seed, fixture, execution configuration and
deterministic guest stop condition, not `sleep` or `wait-idle` wall-clock timing.

## Integration gates

1. Claude confirms the reconciled plan and ownership boundary.
2. Capture a clean committed baseline for a separate implementation worktree.
3. Implement board selection and facade there; leave active Pi work untouched.
4. Rebase or merge committed Pi changes before comparing behavior. Do not interpret
   missing uncommitted fixes as regressions introduced by the facade.
5. Run workspace tests and Pi deterministic/threaded serial boots. Check serial input,
   reset and affected scripts. Snapshot/browser/GPU/Ubuntu checks follow the paths
   changed, as enumerated in Claude's review.
6. Integrate only after reviewing the diff and evidence. CPU profile/GICv3 changes
   remain a later coordinated slice, because they touch the active CPU work.

No runtime checks were run for this documentation-only preparation. Neither this
contract nor Claude's review is evidence of successful Orange Pi boot.
