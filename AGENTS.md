# Working in this repository (all agents: Claude Code, Codex, others)

Several agents work on sbc-emu at the same time. Everything reaches `main` through a pull
request; nobody pushes to `main` directly. GitHub cannot enforce this on this plan, so it is
the rule here.

## Branches and worktrees

- One agent, one branch, one git worktree. Never work in another agent's worktree or commit
  to its branch: `git worktree list` shows who is where.
- Branch names: `<agent>/<topic>`, e.g. `claude/firefox-glyphs`, `codex/orangepi5-usb`.
- Start from the current `main`:
  ```sh
  git fetch origin
  git worktree add ../sbc-emu-<topic> -b <agent>/<topic> origin/main
  ```
- Keep a branch small and about one thing; several small PRs merge more easily than one big
  one.

## Pull requests

1. Rebase on `origin/main` before opening (and again if `main` moved): `git rebase origin/main`.
2. Before pushing, check that nothing broke:
   - `cargo test --workspace`
   - `cargo build --release`
   - the browser build: `"$(rustup which cargo)" check --target wasm32-unknown-unknown -p sbc-emu-wasm`
   - for changes to a board, its boot scripts (`scripts/*.txt`); for GPU changes, the GL suite
     (`scripts/gpu-gltest.txt` with `tools/gpu_server.py`, and `tests/gpu/run-snap-mesa.sh`)
3. Open the PR with `gh pr create --base main`. In the description, say:
   - what changed and why;
   - how it was tested (commands and results);
   - what is known not to work.
4. Merge with a merge commit or rebase, never a force-push to `main`. Delete the branch after.
5. A PR that touches shared crates (`emu-core`, `arm-a64`, `sbc-net`, `cli`, the board
   facade) names the other agents' areas it affects, so their owners can look before merging.

## Areas

Who is working where right now (update this table when you start or finish something):

| Area | Crates / paths | Agent |
|---|---|---|
| Raspberry Pi 4: SoC, GPU (V3D), display, Ubuntu | `bcm2711/`, `v3d/`, `cli/src/vnc*.rs`, `web/` | Claude Code |
| Multi-board foundation, Orange Pi 5 (RK3588S) and its Mali-G610 GPU (`docs/orangepi5.md`, `docs/orangepi5-gpu.md`) | `sbc-machine/`, `rk3588/`, `boards/` | Claude Code (taken over from Codex, 2026-11-19) |
| Orange Pi One (H3), its Mali-400 GPU and the 32-bit core | `allwinner-h3/`, `mali400/`, `arm-a32/`, `cli/src/orangepi_one.rs` | Claude Code |
| RK3288 / Tinker Board (plan: `docs/rk3288-tinker-board-plan.md`) | `rockchip-rk3288/` (storage, I²C, PMIC: `dwmmc.rs`, `i2c.rs`, `rk808.rs`), `cli/src/tinkerboard.rs`; `arm-a32` changes go through its owner | Claude Code (the Orange Pi One session, through a sub-agent on `claude/tinkerboard-*` branches) |
| RK3288 panel board profile on the RK3288 (`docs/panel-board.md`); next: the pads (the STM32 on USB) | `rockchip-rk3288/` (display: `vop.rs`, `dsi.rs`; RGA: `rga.rs`; audio: `i2s.rs`; GPIO; the panel board's own devices), `peripherals/` (its chips: PMICs, RTC, touch, `sdio.rs`, `bcm43455/`, `pl330.rs`), `cli/src/panelboard.rs`, `docs/panel-board*.md` | Claude Code (the Orange Pi One session) |
| Shared device models (`peripherals/`: GIC, UART, I²C chips, timebase); used by the H3 and RK3288 machines, open to every board | `peripherals/` | Claude Code (the Orange Pi One session); changes affect the H3 and RK3288 boards |
| Orange Pi RV (StarFive JH7110) and the RV64 core | `riscv64/`, `starfive-jh7110/`, `cli/src/orangepi_rv.rs`, `docs/orangepi-rv.md` | Claude Code |
| Mali-T760 (Midgard) GPU of the RK3288 (`docs/mali-t760.md`) | `midgard/`, its hookup in `rockchip-rk3288/src/machine.rs`, `scripts/mali-t760-*` | Claude Code (the Orange Pi One session) |
| RISC-V: RV32 core (M/S/U, Sv32) and the ESP32-S31-Function-CoreBoard-1 Linux board (`docs/esp32s31.md`); the RV64 boards are in the Orange Pi RV row | `riscv-rv32/`, `espressif/` | Claude Code (taken over from OpenCode) |

## Never

- Force-push to `main`, or rewrite history others have pulled.
- Commit OS images, snapshots or build output (`cache/`, `target/`, `out/`, `web/assets/` are
  ignored for that reason).
- Run two emulators with `--sd-persist` on the same image (the emulator refuses).
