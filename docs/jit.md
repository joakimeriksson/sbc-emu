# The JIT

`arm-a64/src/jit.rs`. Guest and host share the A64 instruction set, so integer and FP/SIMD
data-processing instructions are emitted as themselves with guest registers renamed to the host
registers that cache them — the host computes exactly the architected result, flags included.
Inside a block guest X registers and SP live in host x0–x8/x22–x28 and V registers in v16–v31
(loaded on first use, written back before exits and interpreter calls); the guest NZCV and FPSR
stay in the host's, also across linked branches and the dispatcher. A block continues past short
forward conditional skips (their taken side leaves through a side exit after the block). Loads and
stores probe the core's micro-TLB inline (about 10 host instructions: the entry holds PA − VA, the
access is `ldr rt, [x21, x14]`); the slow path after the block first refills the micro-TLB through
a shared stub and only then calls the interpreter; system instructions, exception generation and
ERET call the interpreter and end the block. FP/SIMD instructions run natively only
after the interpreter has proven it implements them (so extensions the emulated Cortex-A72 lacks
stay undefined). Blocks are keyed by virtual and physical PC, EL, SPSel and FP enable, and are
discarded on `IC IVAU`/`IC IALLU` (CTR_EL0.DIC = 0 obliges software to issue them). Every CPU test
runs with it too: `SBC_JIT=1 cargo test -p arm-a64`. `--no-jit` turns it off.

What makes it fast (Raspberry Pi OS cold boot to `login:`, measured step by step on an M5 host
with `--no-display`; single runs, some next to other jobs, so ±10%):

| Step | `login:` after |
|---|---|
| block JIT, single-entry micro-TLBs, every block returns to the machine loop | 150 s |
| 64-entry direct-mapped micro-TLBs (fetch/read/write, in the interpreter and inline in compiled code); PAC/BTI hints, barriers, `MRS SP_EL0` native | 78 s |
| block chaining: block exits jump through a shared dispatcher (fetch micro-TLB + front cache) into the next compiled block while `instret < chain_until` (the machine's next device deadline) | 76 s |
| SIMD&FP loads/stores, PRFM and `MRS TPIDR_*` native; MRS and data-cache operations no longer end blocks | 68 s |
| direct linking: an exit to a constant target on the same page branches straight into that block (patched in when it is compiled; same-page links die with the page, so they never need unlinking) | 53 s |
| four cores, Bluetooth and USB devices added since (the measurements below start here) | 56.5 s |
| a fetch micro-TLB miss is filled where the compiled code is looked up (it used to send one instruction to the interpreter); a block that is not hot yet runs in the interpreter to its end, and only block starts count towards compiling (mid-block PCs no longer become overlapping blocks: 2.4 → 1.65 M compiles) | 52.8 s |
| 256-entry micro-TLBs (fetch misses 110 M → 33 M; more would push `Cpu` fields that compiled code reaches by immediate offsets out of range) | 46.5 s |
| native FMOV between X/W and D/S registers, LDNP/STNP, DC ZVA (64-byte zeroing), DUP from a general register, one-register LD1/ST1 (the most frequent instructions compiled blocks still handed to the interpreter) | 42.4 s |
| ADMA2 on the SD card (the driver moved every sector through the data port, 4 bytes per load) | 36.9 s |
| main in November 2026 (V3D, display and device work since; the next row's baseline) | 29.6 s |
| guest registers cached in host registers (16 X, 16 V) instead of loaded and stored around every instruction; load/store slow paths out of line; the micro-TLBs 2-way set associative with a hashed index (python3's data and its mmap heap shared a direct-mapped slot: ~10 M interpreter fallbacks per second); an enter stub instead of a prologue per block, with instret and the RAM base in host registers and NZCV live across linked blocks | 26.4 s (17.5 s with `--threads multi`) |
| main on 20 November 2026 (the Orange Pi 5 work since; the next row's baseline, measured next to it) | 24.1 s (16.4 s) |
| FPSR live in the host register across blocks (each `MRS`/`MSR FPSR` costs 3–4 ns on Apple cores; blocks with FP saved and restored it at every boundary); micro-TLB entries hold PA − VA and the set index is the page number's low bits, so pages P and P + 1 never share a set and a page-crossing access needs no separate check (about 10 host instructions per load instead of 17); a miss refills the entry through a shared stub (`h_tlb`, no interpreter) and retries; interpreter calls through a shared stub (a call site went from ~37 instructions to ~6); blocks continue past short forward conditional branches; direct branches into another page linked too, behind a check that the fetch micro-TLB still maps the target page to the same physical page (42% of dispatcher entries in python3) | 19.0 s (13.5 s) |

Blocks average about 5 guest instructions; a chain now runs about 100 before it returns to the
machine loop. A chain stops at the budget/deadline and whenever an interpreted instruction may need
the machine's attention (`chain_stop`: device access, MSR, cache/TLB maintenance, exceptions, ERET).

Debugging switches: `SBC_JIT_VERIFY=1` re-runs every block in the interpreter from the same state
(stores rolled back; blocks whose code changed before an `IC`, or that touched a device, are
skipped) and reports the first mismatch; `SBC_JIT_PROFILE=1` prints the instruction words compiled
blocks still hand to the interpreter; `SBC_JIT_CHAIN=0` turns chaining and linking off;
`SBC_JIT_SUPERBLOCK=0` ends blocks at every conditional branch;
`SBC_JIT_MAP=FILE` writes the host address, size and guest PC of every compiled block (to attribute
host profiler samples, e.g. from `samply`); `SBC_JIT_DUMP=<pc>[,<pc>...]` writes the guest and host
instruction words of the blocks at those PCs to `jit-<pc>.guest`/`.host`;
`SBC_JIT_WATCH=<page>` traces compiles and invalidations on one physical page.

## In the browser

`arm-a64/src/jit_wasm.rs` compiles a hot **region** into its own small WebAssembly module
(imports the emulator's memory and function table, exports one function) which the worker
instantiates and installs in the table. A region is the hot entry block plus the basic blocks
reachable through constant branches on the same page (up to 24): one function with a `loop` +
`br_table` dispatch, so loops and local control flow branch inside the function instead of going
back to Rust (WebAssembly code cannot be patched, so this replaces the native JIT's direct links).
Exits leaving the region return to a chaining loop in `jit_step` that calls the next block from the
front cache. Translated natively: integer data processing (incl. extended-register add/sub,
CCMP/CCMN, variable shifts, REV/RBIT/CLZ, ADC/SBC, UMULH/SMULH), flags, conditional select,
branches, integer loads/stores (inline micro-TLB probe, direct RAM access), NOP-class hints,
barriers, PRFM and thread-pointer/`SP_EL0` reads; everything else calls the interpreter for one
instruction. Execution counting before compilation uses a direct-mapped table (no hashing on the
interpreter's path).

On by default (`pi4.html?jit=0` or `gpu.html?jit=0` turns it off). Raspberry Pi OS cold boot to `login:` in the
browser build (`tools/wasm-smoke-sd.mjs`, Node 26, M5, runs alone):

| Step | `login:` after |
|---|---|
| block per module, every block back to the machine loop | 313 s |
| direct-mapped hot counters, chaining loop in `jit_step` | 252 s |
| more native instructions (hints, CCMP, extended add, shifts, ADC, MULH, ...) | 229 s |
| regions (loop + `br_table` per page-local control flow) | 160 s |

The interpreter alone takes about 360 s; the busybox kernel comes up after 1.9 s (13.5 s
interpreted). Checks:

- `node tools/wasm-jit-selftest.mjs` runs A64 snippets through the wasm build with the JIT off and
  on and compares registers and flags;
- `SBC_JIT=1 SBC_JIT_VERIFY=1 node tools/wasm-smoke.mjs 30` (or `wasm-smoke-sd.mjs IMAGE SECS`)
  re-runs every compiled block in the interpreter (stores are rolled back) and prints the first
  mismatch, with the block's code; `SBC_JIT_PROFILE=1 node tools/wasm-smoke-sd.mjs ...` lists the
  instructions compiled code still hands to the interpreter, `SBC_JIT_HOT=n` sets the compile
  threshold and `SBC_WASM=path` picks another build (for before/after comparisons).

The Tinker Board S runs in the same build: `web/tinker.html` boots the unmodified Armbian SD
image (served like the Pi's, `tools/wasm-build.sh` links it from the fixture) or Debian's
kernel with a BusyBox initramfs; HDMI is drawn on a canvas, and input is the serial console
until the board has USB. Its 32-bit core has a WebAssembly backend of its own
(`arm-a32/src/jit_wasm.rs`): the native A32/T32 JIT's instruction coverage, compiled per region
as above, with the guest flags in wasm locals. In Node 26 on an M5:

| | interpreter | JIT |
|---|---|---|
| Debian kernel to the BusyBox shell (`tools/wasm-smoke-tinker.mjs`) | 241 s (16 MIPS) | 5.7 s |
| Armbian to `login:` (`tools/wasm-smoke-tinker.mjs 1500 --sd IMAGE`) | | 257 s (native: 218 s; both wait about 2 minutes for systemd's random seed) |

`node tools/wasm-a32-jit-diff.mjs` runs `arm-a32/tests/jit_diff.rs`'s random A32 and Thumb cases,
straight-line and in loops, through the wasm backend against the interpreter.

## The RV64 JIT (Orange Pi RV)

`riscv64/src/jit.rs` drives a separate JIT for the RISC-V core with two code generators: RV64 to
AArch64 (`jit_a64.rs`) and RV64 to WebAssembly (`jit_wasm.rs`). Its regions are keyed by physical
address and shared by the four harts, stay valid until `fence.i` (then are compared with the bytes
they were compiled from), and probe a per-mode micro-TLB for loads and stores. Natively, regions
are linked through a per-hart jump cache and keep the guest registers they use most in host
registers. Four busy shell loops run 16 times faster than in the interpreter (about 2.2 G RISC-V
instructions per second on an M5). The design, its tests and timings are in
[orangepi-rv.md](orangepi-rv.md#the-jit).
