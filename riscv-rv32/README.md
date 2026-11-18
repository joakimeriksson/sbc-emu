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

This is **RV32 machine-mode code**, not an RV64 Linux CPU: it has no S/U privilege
modes, Sv39 MMU, SBI, standard PLIC/CLINT or D extension. Those belong to the later RV64
work. Local integration changes are limited to workspace/Cargo metadata and attribution;
the instruction implementation and tests otherwise retain the upstream source.
