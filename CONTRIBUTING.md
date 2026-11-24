# Contributing to sbc-emu

Contributions are welcome, from people and from coding agents. Everything reaches `main`
through a pull request; nobody pushes to `main` directly.

## Making a change

- One change, one branch. Start it from the current `main`:
  ```sh
  git fetch origin
  git switch -c my-topic origin/main      # or a worktree: git worktree add ../sbc-emu-my-topic -b my-topic origin/main
  ```
- Keep a branch small and about one thing; several small pull requests merge more easily than
  one big one.
- Rebase on `origin/main` before opening the pull request, and again if `main` has moved.

## Before you push

- `cargo test --workspace`
- `cargo build --release`
- the browser build: `rustup target add wasm32-unknown-unknown` once, then
  `"$(rustup which cargo)" check --target wasm32-unknown-unknown -p sbc-emu-wasm`
- for a change to a board, its boot scripts (`scripts/*.txt`, run with `sbc-emu run ... --script`;
  each script's header has the command); for a change to a GPU, its test scenes (for the
  Raspberry Pi 4, `scripts/gpu-gltest.txt` with `tools/gpu_server.py`, and
  `tests/gpu/run-snap-mesa.sh`; each board's GPU page names its own)

The toolchain is pinned in `rust-toolchain.toml`; rustup installs it on first use. CI runs the
tests on macOS (Apple silicon) and Linux x86-64, and checks the browser build.

## The pull request

Open it against `main` (`gh pr create --base main`, or on GitHub). In the description, say:

- what changed and why;
- how it was tested (commands and results);
- what is known not to work.

A change to a shared crate (`emu-core`, a CPU core, `peripherals`, `sbc-net`, `sbc-machine`,
`cli`) says which boards it affects. Pull requests are merged with a merge commit or a rebase;
`main`'s history is never rewritten.

## Never commit

- operating system images, vendor firmware, snapshots or build output (`cache/`, `target/`,
  `out/` and `web/assets/` are ignored for that reason). The fixture scripts fetch guests from
  their publishers, pinned and checksummed; add a script like them instead.
- code you may not license as below.

## License

sbc-emu is BSD 3-Clause ([LICENSE](LICENSE)). By contributing you agree that your contribution
is licensed under it, except in `riscv-rv32/` and `espressif/`, which keep esp32sim's MIT
license ([README](README.md#license)).

## Coding agents

[AGENTS.md](AGENTS.md) has the extra rules for agents working on this repository at the same
time (branch names, worktrees, who is working in which area).
