# RV32 interpreter reused from esp32sim

This crate and its tests were imported from
[`mikroverk/esp32sim`](https://github.com/mikroverk/esp32sim) at commit
[`d043ff1dd3e53aca18a23f4f2b18bfb038a57667`](https://github.com/mikroverk/esp32sim/commit/d043ff1dd3e53aca18a23f4f2b18bfb038a57667).
The upstream project is MIT-licensed; its copyright and license are preserved in
[`LICENSE`](LICENSE).

The import provides RV32IMC, RV32IMAC and RV32IMAFC variants used by the ESP32-C3,
C6, P4 and S31, including the instruction decoder, interpreter, decoded blocks,
soft-float implementation, disassembler and hermetic test corpus.

The nested `esp32sim-emu-core` package is the upstream 32-bit support API required by
this code. It is private to this import and is not used by existing sbc-emu machines;
the main `emu-core` package is unchanged. Keeping this small compatibility layer avoids
rewriting the imported implementation and tests during the initial import.

The nested support package now lives in `../espressif/emu-core` (package `esp32sim-emu-core`),
shared with the vendored ESP32-S31 SoC.

Changes in sbc-emu: the S31 profile (`Cpu::new_esp32s31`, `Cpu::privileged`) adds M/S/U
privilege, trap delegation, `sret`, `sfence.vma`, Sv32 translation (`src/mmu.rs`) and the S31's
CLIC-mode `scause`/`sret`, enough for Espressif's OpenSBI, U-Boot and Linux
(`docs/esp32s31.md`). The C3, C6 and P4 profiles keep their machine-mode-only behaviour. It is
still RV32 only: no RV64, Sv39, standard PLIC/CLINT or D extension.
