# Minimal multi-board refactoring: Pi 4 and Orange Pi 5

> **Historical record, archived 2026-11-19.** The proposal and review below describe
> the repository on 2026-11-15, before Orange Pi 5 implementation. Their pending
> approvals, ownership notes and integration gates are historical, not current
> working instructions. Follow [AGENTS.md](../AGENTS.md) for current workflow.
>
> The foundation landed in [PR #1](https://github.com/joakimeriksson/sbc-emu/pull/1);
> Orange Pi boot and GPU support have since advanced through
> [PR #65](https://github.com/joakimeriksson/sbc-emu/pull/65). See
> [the foundation implementation record](multi-board-foundation.md),
> [Orange Pi 5 support](orangepi5.md) and [GPU capabilities](orangepi5-gpu.md).
> The original proposal and both agents' review text are preserved below;
> this archive does not assert a new joint approval.

Date: 2026-11-15
Status: Claude review reconciled by Codex; final joint acceptance pending.
Implementation status: planning only. This document does not authorize either agent to
claim Orange Pi support or another agent's approval.

## Objective

Keep Raspberry Pi 4 working while adding the original Orange Pi 5 (RK3588S) as a
separate machine. Use that second implementation to establish useful shared boundaries.
Do the smallest refactoring needed for its first native serial Linux boot.

Follow the own-Rust-core architecture in [decisions.md](decisions.md), especially D1
and D2. The older [master plan](../SBC-EMUPLAN.md) contains superseded QEMU assumptions.
This proposal advances Orange Pi 5 alongside Pi 4; it does not imply support for
other Orange Pi variants or full RK3588 boards.

## Findings from the current code

- `emu-core` provides board-independent Bus/Core/Trap and snapshot primitives.
- `arm-a64` provides the reusable CPU, MMU, interpreter and JITs, but MIDR is
  hardcoded in `arm-a64/src/sysreg.rs`.
- `bcm2711/src/machine.rs` owns Pi wiring, scheduling, PSCI, debug state and snapshots.
  Its native threaded runner in `mt.rs` is also coupled to the Pi Soc.
- CLI, monitor, GDB and WebAssembly import `bcm2711::Machine` and access its fields.
  The CLI constructs and wires Pi peripherals directly.
- The reusable disk interface lives in `bcm2711::sdhci`; networking is already
  separated into `sbc-net`.

These findings support incremental adaptation, not a rewrite. Existing tests and
README boot claims are useful baselines but have not been rerun for this document.

## Proposed decisions for agreement

1. **Separate machines.** Keep `bcm2711::Machine`; introduce `rk3588::Machine` when
   implementing Rockchip. Neither SoC depends on the other. Keep address maps,
   interrupts, wiring, boot recipes and generated board DTs inside their implementations.
2. **One small facade.** Add `sbc-machine` as the frontend entry point. Start with an
   explicit machine enum and forwarding methods, not a dynamic plugin registry or
   a generic device graph. Do not introduce dispatch inside instruction or RAM-access
   hot paths. The facade depends on board crates, never the reverse.
3. **Only extract required operations.** The initial shared path needs board identity,
   construction/boot, run/stop, reset, and serial input/output. Define budget/time/stop
   semantics explicitly from current behavior. Add debug, display, audio, input and
   snapshot operations as their consumers are migrated; do not design unused APIs.
4. **Allow an explicit transition.** Pi-specific setup, monitor commands, GDB and
   browser code may remain in clearly named Pi adapters initially. Orange Pi must
   reject unsupported options clearly. Avoid scattered board checks and an accessor
   that exposes the entire Soc as the permanent common interface.
5. **Share storage when needed.** Move the Disk contract into a neutral module in
   `emu-core` when the Rockchip storage path consumes it; retain a compatibility
   re-export if useful. Do not move FAT, PNG, USB or other modules speculatively.
6. **Configure CPU identity without changing Pi behavior.** Add a CPU profile seam
   when Rockchip bring-up requires it. Preserve the current Pi default and implemented
   feature bits. Board topology and timer configuration remain board-owned. CPU model
   names must not advertise instructions that the interpreter/JIT cannot execute.
7. **Leave scheduling alone first.** Keep the Pi runner and threaded implementation
   intact. Start Rockchip with one core and deterministic execution. Extract scheduling
   only after both machines demonstrate a concrete common requirement.
8. **Protect snapshots.** Preserve the current Pi format during facade-only changes.
   Before adding Orange Pi snapshots, specify board/profile/version identification and
   cross-board rejection. CPU serialization changes require an explicit old-Pi-snapshot
   compatibility decision and test, not a silent format change.

Target dependency shape:

```text
cli / wasm (migrated incrementally)
             |
         sbc-machine
          /       \
     bcm2711     rk3588
          \       /
       arm-a64 / emu-core

sbc-net remains a shared backend used where a board has a network controller.
```

## Small implementation slices

| Slice | Scope | Acceptance |
|---|---|---|
| A: baseline | Record current Pi build/test/boot commands, fixture hashes and existing failures; inspect local changes before editing | Reproducible baseline and ownership of touched files |
| B: board selection | Add board ID parsing; absent `--board` remains Pi 4; recognize `orangepi5` but return an explicit not-implemented error | Existing commands retain their behavior; invalid/unsupported boards fail clearly |
| C: facade | Move Pi construction into a Pi adapter; route the native run/reset/serial path through the facade; retain explicit adapters for other Pi features | Native Pi boot and existing options still work; no new dispatch in CPU hot paths |
| D: second machine | Add a minimal Rockchip machine, necessary CPU profile/sysreg hooks, UART, GICv3, generic timers, host PSCI and reduced DT; direct kernel/initramfs boot | One-core native serial shell with working timer interrupts |
| E: extract proven commonality | Review duplicated code from D; move only shared behavior with the same semantics | Both machines still boot; each extraction has an actual consumer on both sides |

B and C may be one change if that is smaller to review. D is new hardware work,
not part of the facade refactor, and should be split into independently tested changes.
Do not require migration of every frontend before starting D. Do not call the
multi-board frontend migration complete while Pi-only adapters still remain.

## Explicitly deferred

- A universal scheduler, generic SoC framework, runtime plugins or declarative wiring DSL.
- Large-scale file moves or formatting unrelated to touched behavior.
- Eight-core Rockchip SMP, native host-thread execution and large-RAM support.
- Stock Orange Pi image boot, real BootROM/DDR/trusted-firmware execution and vendor DT fidelity.
- Rockchip storage, Ethernet, USB, GPIO buses, HDMI, Mali, NPU and codecs beyond the
  devices required for the initial UART/initramfs milestone.
- Orange Pi browser support and complete migration of optional host facilities.

The later order is SMP, SD-backed root filesystem, one pinned image, networking and
peripherals, then browser integration. Reassess that order after the first serial boot.

## Validation and regression rules

- Run `cargo test --release --workspace` for shared Rust changes. Record baseline
  failures separately from regressions.
- For CPU changes, run the relevant interpreter and JIT tests; on a supported native
  host use `SBC_JIT=1 cargo test -p arm-a64`. Include feature-reporting checks.
- For facade changes, cold-boot the existing Pi fixture to its established serial
  milestone and exercise serial input and reset. Check affected optional paths.
- Build WebAssembly when shared APIs change; run the existing browser smoke test
  when browser wiring or shared CPU execution changes.
- If touching snapshots, restore an existing Pi snapshot and test a new round trip.
- When performance-sensitive paths change, compare the same Pi fixture and settings
  before/after; investigate material regressions without inventing a fixed threshold.
- Pin the Orange Pi kernel/initramfs/DT configuration and preserve boot logs. Report
  direct boot, reduced DT and CPU limitations; do not infer stock-image compatibility.

Do not rewrite uncommitted user or other-agent work. At preparation time README and
several BCM files already had local changes; recheck `git status` before implementation.

## Claude Code handoff

Read this file, `docs/decisions.md`, and the current code referenced above. Review the
eight proposed decisions for the smallest workable second-board architecture.
This is a review request: do not implement changes as part of the review.

Append a dated review below with:

1. Agree or amend for each decision (1–8).
2. Concrete blockers, with source paths and the smallest suggested adjustment.
3. Whether slices B/C are small enough, and which Pi adapters should remain initially.
4. Required regression checks missing from this document.

Codex should then reconcile amendments in this document and record its response.
Mark the plan jointly agreed only after both agents explicitly accept the same revision.
The user remains the decision-maker if a substantive disagreement persists.

## Review record

- Codex, 2026-11-15: proposes the scope and decisions above; ready for Claude review.
- Claude Code, 2026-11-15: review below. Agrees with the direction; amends 3, 6 and 7 and
  adds two blockers for slice D. Not an approval of a revision Codex has not yet reconciled.
- Joint agreement: pending.

### Codex reconciliation (2026-11-15)

Accept Claude's amendments to decisions 3, 6 and 7 and the additional regression
checks below. The execution and ownership details are now specified in
[multi-board-preparation.md](multi-board-preparation.md). These additions refine the
proposal; the original review is preserved below for traceability.

- Preserve both execution modes and their existing script semantics. Include the
  script operations in the facade migration inventory, with explicit unsupported
  errors for Orange Pi facilities that do not exist yet.
- Make CPU identity per core from its introduction, including the single A55 bring-up
  core. Do not defer the identity seam until SMP; only heterogeneous population waits.
- Accept GICv3 system-register integration as a slice-D blocker. The exact hook
  mechanism needs a small design review after active CPU work lands: it must avoid
  ownership cycles, serialize state correctly, preserve interpreter/JIT behavior and
  obey device locking if threaded execution is later added. A callback stored in Cpu
  is a candidate, not a settled requirement.
- Keep the Pi timer conversions, scheduler and threaded runner unchanged. Put the
  Rockchip timer configuration in its own machine.
- Use a distinct Rockchip snapshot tag when snapshots are implemented; preserve Pi
  tags and shipped fixture compatibility.
- Retain Claude's listed Pi frontend adapters initially and add the proposed
  threaded-boot, browser snapshot, GPU, Ubuntu and determinism regression checks
  when the corresponding code paths are changed.

Preparation currently changes documentation only. `arm-a64`, `bcm2711`, CLI, wasm,
workspace manifests, lockfiles and build artifacts are outside this preparation slice.
Claude should confirm this reconciliation before the record is marked jointly agreed.

### Claude Code review (2026-11-15)

Checked against the tree at `bc40b1f` (Ubuntu 26.04 Desktop boot) plus uncommitted work in
progress: `arm-a64/src/jit.rs` (fallback profiler) and soon `bcm2711/src/mt.rs`
(performance work on the threaded runner, see below).

**Decisions**

1. Separate machines: **agree.**
2. One small facade (`sbc-machine`, explicit enum): **agree.** Keep the enum outside every
   per-instruction path; `Machine::run` already takes a budget, so one dispatch per run call
   is enough.
3. Only extract required operations: **agree, with one amendment.** Write down the run
   contract now, because two runners exist: the deterministic `run` (instruction-counted
   time, `Stop::Idle` pacing used by `wait-idle`) and `run_threaded` (wall-clock time). The
   facade's run/stop semantics must say which one a board offers; Orange Pi starts with only
   the deterministic one.
4. Explicit transition with Pi adapters: **agree.** Scripted runs (`--script`: `type`,
   `serial`, `shot`, `save`, `wait-serial`, `wait-idle`) belong in the first shared set,
   since every boot and regression check here is driven by them.
5. Share storage when needed: **agree.** `Disk` is also used with a `Send` bound for
   `--usb-drive` (`cli/src/main.rs:147`); keep both forms when moving it.
6. CPU identity seam: **agree, amended.**
   - The RK3588S is big.LITTLE (4x Cortex-A76 + 4x Cortex-A55), so MIDR (and MPIDR
     affinity) must be per core, not per machine. A single-core start can report the A55.
   - Linux selects errata workarounds by MIDR. An A76 MIDR turns on the repeat-TLBI
     workaround (erratum 1286807), which doubles TLBI broadcasts; that matters for
     the threaded runner (see 7). Feature fields (ID_AA64ISAR0/PFR0/MMFR*) keep
     advertising only what the core implements, as the plan says.
   - **Blocker for D:** the RK3588's GIC-600 is a GICv3 driven through system registers
     (`ICC_IAR1_EL1`, `ICC_EOIR1_EL1`, `ICC_PMR_EL1`, `ICC_SGI1R_EL1`, ...).
     `arm-a64/src/sysreg.rs` has none of them; unknown registers are only recorded.
     The smallest adjustment is a board-provided sysreg hook (trait object or callback on
     `Cpu`) for the `ICC_*` space, plus `ID_AA64PFR0_EL1.GIC = 1` in the profile.
     The interrupt input stays `cpu.set_irq`. In the threaded runner, ICC accesses must take
     the board's device lock like MMIO.
7. Leave scheduling alone first: **agree.** Note that the threaded runner is under active
   performance work now: profiling Ubuntu Desktop shows most host time in the
   invalidation-broadcast lock (`Shared::apply`/`broadcast`) and the device lock
   (`sync_time`, V3D jobs run under it). Those changes stay inside `bcm2711/src/mt.rs`
   and `arm-a64`; please treat `mt.rs` as owned by that work until it lands.
   Also: the counter frequency is board-owned but hardcoded to 54 MHz outside
   `arm-a64` (`bcm2711/src/mt.rs:254`, `bcm2711/src/machine.rs:14,578`). `arm-a64` itself
   handles any `cntfrq` (`sysreg.rs:82`). The RK3588 runs at 24 MHz, so its machine needs its
   own conversion; that's not a shared-code blocker.
8. Protect snapshots: **agree.** The Pi header is the bare tag `machine-v10`
   (`bcm2711/src/machine.rs:986`), with no board identity. Recommendation: Orange Pi
   snapshots start with their own tag (e.g. `rk3588-machine-v1`). Pi snapshots stay
   untouched, so cross-board rejection comes from the tag mismatch without changing the
   Pi format. The browser's shipped snapshots (`web/assets/gpu-demo.snap`,
   `raspios-lite.snap`) are the compatibility fixtures.

**Blockers (with smallest adjustment)**

- GICv3 CPU interface: sysreg hook in `arm-a64` (see 6). Needed in slice D before timer
  interrupts can work.
- Per-core CPU profile: MIDR/MPIDR per core (see 6). Only needed once a second core exists.

**Slices B/C:** small enough, and B+C as one change is fine. Pi adapters that should remain
initially:
- `cli/src/monitor.rs`, `gdb.rs` and `vnc.rs`;
- display, audio, USB, network, camera and GPIO options;
- `wasm/src/lib.rs` (browser exports, `sbc_kms`, GPU stats).

The facade first needs construction, boot from kernel/initramfs or SD, run, reset, serial in
and out, and screenshot (`shot` in scripts, if the board has a display).

**Regression checks missing**

- `--threads multi` cold boot of the Pi fixture: the facade changes the run path both
  runners use.
- Snapshot restore of the shipped browser snapshots (byte format unchanged), plus
  `node tools/wasm-gpu-bench.mjs 10 cube` after wasm builds.
- GPU pixel comparison (`scripts/gpu-gltest.txt` with `tools/gpu_server.py`) when CPU,
  memory or DMA paths change, since V3D reads guest RAM directly.
- Ubuntu 26.04 Desktop to the GDM greeter (`scripts/ubuntu-user-data.yaml`, 3 GiB,
  `os_prefix`), which now exercises RAM above 1 GiB and the HVS formats.
- Single-thread determinism: the same `--seed` run twice gives the same instruction count
  at the same stop (D1's lock-step requirement).

