# Working in this repository (all agents: Claude Code, Codex, others)

Several agents work on sbc-emu at the same time. Everything reaches `main` through a pull
request; nobody pushes to `main` directly. GitHub cannot enforce this on this plan, so it is
the rule here. [CONTRIBUTING.md](CONTRIBUTING.md) has the rules for everyone; this file adds
what agents working in parallel need.

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
| Raspberry Pi 3 (BCM2837), plan: `docs/rpi3-plan.md` | `bcm2837/`, `cli/src/pi3.rs`, `scripts/rpi3-*`, its VideoCore IV 3D GPU `vc4-3d/` (`docs/raspberry-pi-3-gpu.md`); its browser page `web/rpi3.html` (`tools/wasm-smoke-rpi3.mjs`); the 3B's LAN9514 Ethernet `emu-core/src/smsc95xx.rs`; uses `bcm2711/`'s blocks (changes there go through the Raspberry Pi 4 row) | Claude Code |
| Multi-board foundation, Orange Pi 5 (RK3588S) and its Mali-G610 GPU (`docs/orangepi5.md`, `docs/orangepi5-gpu.md`) | `sbc-machine/`, `rk3588/` | Claude Code |
| Orange Pi One (H3), its Mali-400 GPU and the 32-bit core | `allwinner-h3/`, `mali400/`, `arm-a32/`, `cli/src/orangepi_one.rs` | Claude Code |
| RK3288 / Tinker Board (`docs/tinkerboard.md`; plan: `docs/history/rk3288-tinker-board-plan.md`) | `rockchip-rk3288/` (storage, I²C, PMIC: `dwmmc.rs`, `i2c.rs`, `rk808.rs`), `cli/src/tinkerboard.rs`, the RK3288's display (`vop.rs`, `dsi.rs`), RGA, audio (`i2s.rs`, `peripherals/src/pl330.rs`), GPIO, and its browser page `web/tinker.html` (with `wasm/`); `arm-a32` changes (also its WebAssembly JIT, `jit_wasm.rs`) go through its owner | Claude Code (the Orange Pi One session, through a sub-agent on `claude/tinkerboard-*` branches) |
| Shared device models (`peripherals/`: GIC, UART, I²C chips, timebase); used by the H3 and RK3288 machines, open to every board | `peripherals/` | Claude Code (the Orange Pi One session); changes affect the H3 and RK3288 boards |
| Orange Pi RV (StarFive JH7110) and the RV64 core (and its JIT to AArch64, x86-64 and WebAssembly, `riscv64/src/jit*.rs`) | `riscv64/`, `starfive-jh7110/`, `powervr/`, `tests/gpu/pvr/`, `cli/src/orangepi_rv.rs`, `docs/orangepi-rv.md` | Claude Code |
| Mali-T760 (Midgard) GPU of the RK3288 (`docs/mali-t760.md`) | `midgard/`, its hookup in `rockchip-rk3288/src/machine.rs`, `scripts/mali-t760-*` | Claude Code (the Orange Pi One session) |
| RISC-V: RV32 core (M/S/U, Sv32) and the ESP32-S31-Function-CoreBoard-1 Linux board (`docs/esp32s31.md`); the RV64 boards are in the Orange Pi RV row | `riscv-rv32/`, `espressif/` | Claude Code |
| NVIDIA Jetson Nano / Tegra X1 (`docs/jetson-nano.md`; plan: `docs/history/jetson-nano-plan.md`); the `ram_base` of `arm-a64` (RAM above physical 0) | `tegra210/`, its GPU `maxwell/`, `maxwell-shader/`, `maxwell-tex/` (`docs/jetson-nano-gpu.md`), `cli/src/jetson_nano.rs`, `scripts/jetson-nano-*`, its GPU demos (`demos/nano-gl/`, `tools/stub_library.py`) | Claude Code |
| NVIDIA Jetson Orin Nano 4GB / Tegra234 and its GA10B (Ampere) GPU (`docs/jetson-orin-nano.md`) | `tegra234/` (its GPU in `tegra234/src/gpu/`; the SM70 decoder in `maxwell-shader/src/decode_sm70.rs`), `cli/src/jetson_orin_nano.rs`, `scripts/jetson-orin-nano-*`, `tests/gpu/orin/`, `tests/gpu/run-orin-*.sh`, `demos/orin-copy/`, `demos/orin-gl/` | Codex (`codex/tegra234-direct-boot`), continued by Claude Code (`claude/orin-cuda`) |

## Never

- Force-push to `main`, or rewrite history others have pulled.
- Commit OS images, snapshots or build output (`cache/`, `target/`, `out/`, `web/assets/` are
  ignored for that reason).
- Run two emulators with `--sd-persist` on the same image (the emulator refuses).
