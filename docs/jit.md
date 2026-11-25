# The JIT

Each CPU core is an interpreter with a block JIT that compiles hot code for the host. The JIT is
on by default; `--no-jit` runs the interpreter alone, and `?jit=0` does the same
on a browser page.

| Core | AArch64 (macOS, Linux) | x86-64 Linux | WebAssembly (browser) |
|---|---|---|---|
| `arm-a64` (Pi 4, Orange Pi 5, Jetson Nano) | `jit.rs` | `jit_x64.rs` | `jit_wasm.rs` |
| `arm-a32` (Orange Pi One, Tinker Board) | `jit.rs`, `jit_t32.rs` | `jit_x64.rs` | `jit_wasm.rs` |
| `riscv64` (Orange Pi RV) | `jit_a64.rs` | `jit_x64.rs` | `jit_wasm.rs` |
| `riscv-rv32` (ESP32-S31) | `jit.rs` | `jit/x64.rs` | `jit.rs` |

On x86-64 macOS no backend is built and the cores interpret. The Jetson Nano's GPU shaders have a
JIT of their own, to WebAssembly in the browser only ([jetson-nano-gpu.md](jetson-nano-gpu.md#shader-jit-browser)).

## The A64 JIT on AArch64 hosts

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
stay undefined); a reserved encoding the interpreter took would reach the host CPU as SIGILL, so
`arm-a64/tests/simd_decode.rs` checks its FP/SIMD decode against LLVM's disassembler. Exclusives
and acquire/release accesses at a misaligned address go to the interpreter, which raises the
Alignment fault (the host's own LDAR/STLR and exclusives would raise SIGBUS). Blocks are keyed by virtual and physical PC, EL, SPSel and FP enable, and are
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

AdvSIMD structure loads and stores run natively too: LD1-LD4/ST1-ST4 of one to four registers,
of one lane, and LD1R-LD4R, with no offset or post-indexed by the size or by Xm. One micro-TLB
probe covers the whole access (up to 64 bytes; one that crosses a page goes to the interpreter);
LD1/ST1 of whole registers become LDR/LDP (STR/STP) of Q or D registers on the cache registers, the
others the host's own instruction (through scratch v0-v3 when they name several registers). Before,
only one-register LD1/ST1 were native: ncnn's YOLOv8n on the Jetson Nano's CPU (`ld1 {v0.4s-v3.4s},
[x8], #64` in every kernel) handed 491 M instructions to the interpreter in 12 frames, 89 M after
(the rest are system instructions, and structure accesses across a page); a frame took 3.1 s of host
time and now takes 0.45 s (the guest's own clock, 1 ns per instruction, says 0.29 s in both).
`arm-a64/tests/jit_neon_ldst.rs` checks every encoding of both groups against the interpreter.

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

## The A64 JIT on x86-64 Linux hosts

`arm-a64/src/jit_x64.rs` translates A64 to x86-64 (x86 cannot run A64 instructions as
themselves). It shares the driver in `jit.rs` (block map and front cache, hotness counters,
direct links as patched `jmp rel32`, invalidation by IC IALLU/IVAU, verify mode, the fallback
profile) and has its own dispatcher and stubs. Native: integer data processing, branches,
loads and stores of every addressing form through the micro-TLB (way 0 inline, way 1, the refill
and the interpreter out of line), exclusives and acquire/release, DC ZVA, FMOV/DUP/UMOV/SMOV
between the register files, constant MOVI, the thread-pointer MRS, and the byte-vector NEON that
glibc's string functions use (CMEQ, CMHS, vector logic, UMAXP/UMINP, SHRN) with SSE2. Everything
else goes to the interpreter one instruction at a time; FP/SIMD registers and FPSR stay in `Cpu`.

Guest registers: rbx = `Cpu`, r13 = guest RAM, r14 = instret, r15 = NZCV; rbp, rsi, rdi and r8–r12
cache guest X registers and SP inside a block. NZCV is kept in x86 form with the carry inverted
(N = SF, Z = ZF, V = OF, C = NOT CF: SUB/CMP give it directly, ADD/CMN add a CMC, logical S forms an
STC), so every A64 condition is one x86 condition code and SBC is a plain SBB. Within a block the
flags stay in EFLAGS until code that would clobber them; then `lahf; seto al` saves them to r15,
and `add al, 0x7f; sahf` brings them back for a consumer. With a host thread per core, full
DMB/DSB become MFENCE, STLR an XCHG and store-exclusive a LOCK CMPXCHG.

Known gaps: FP/SIMD arithmetic beyond the operations above runs in the interpreter; only eight
host registers cache guest registers; x86-64 macOS is not wired up. The interpreter stub leaves
to the dispatcher by dropping its return address (`add rsp, 8` and a jump), which a CET user
shadow stack would refuse on `ret`; Linux enables those only for binaries marked for them, and
this one is not.

Tests: `jit_x64.rs` checks its encoder against llvm-mc; `arm-a64/tests/jit_diff.rs` runs random
instruction sequences (half with the MMU on and a scrambled mapping) on the JIT and the
interpreter and compares registers, flags, V registers, memory and instruction counts; every
arm-a64 test also runs with `SBC_JIT=1`. On an i9-12900F (one host thread unless noted):

| | interpreter | JIT |
|---|---|---|
| Raspberry Pi OS Lite to `raspberrypi login:` | 522 s | 44.4 s (30.3 s with `--threads multi`) |
| Pi 4, Pi kernel + BusyBox initramfs to `pi4#` | 18.7 s | 1.7 s |
| Orange Pi 5, Debian kernel + initramfs to `orangepi5#` | 18.2 s | 1.1 s |
| Jetson Nano, kernel + initramfs to `TEGRA210_BOOT_OK` | 20.6 s | 2.0 s |
| shell loop (300,000 iterations) + `md5sum` of 64 MiB, Pi 4 / Orange Pi 5 / Jetson Nano | 106 / 105 / 101 s | 5.5 / 4.8 / 5.2 s |

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

## The 32-bit Arm JIT on x86-64 Linux hosts

On x86-64 Linux, `arm-a32/src/jit_x64.rs` compiles the Orange Pi One's and the Tinker Board's
A32/T32 code to x86-64. It translates the same regions and instructions as the WebAssembly backend
(and so the same set as the native AArch64 one; VFP, NEON, exclusives and system instructions go
to the interpreter one instruction at a time), with the Thumb decoding and region scan shared in
`jit_decode.rs`. Guest registers stay in `Cpu::r` and are used as memory operands. The guest NZCV
lives in r14 as an image of the host's EFLAGS bits (N = SF, Z = ZF, C = CF, V = OF): ADDS/ADCS
keep the host flags as they are; x86 subtracts produce a borrow, so SUBS/SBCS/RSBS/RSCS complement
CF first and SBC/RSC feed the inverted carry in as the borrow; logical operations with S take N and
Z from the result and C from the shifter. Branches inside a region are direct jumps while the
budget lasts, and region exits chain through a dispatcher like the AArch64 one. The encoder's
output is checked against `llvm-mc` in its unit tests; `arm-a32/tests/jit_diff.rs` (random code,
straight-line and in loops) and `jit_flags.rs` (every condition under every NZCV value, the
add/subtract and shifter-carry edges) compare it with the interpreter.

On an i9-12900F (pinned to the P-cores, the direct-boot fixtures; the interpreter one run, the JIT
the best of three; the shell prompt comes after the same idle waits in both):

| | Orange Pi One, interpreter | JIT | Tinker Board S, interpreter | JIT |
|---|---|---|---|---|
| kernel to the shell prompt | 37.0 s | 4.2 s | 108.3 s | 4.2 s |
| BusyBox `while` loop, 200 000 rounds | 117.2 s | 4.8 s | 138.6 s | 5.3 s |
| `md5sum /bin/busybox` 20 times | 6.1 s | 0.35 s | 5.9 s | 0.30 s |

The unmodified Armbian images pass `scripts/orangepi-one-armbian.txt` (a Thumb-2 kernel, to the
first-login wizard) and `scripts/tinkerboard-armbian.txt` in about 6.5 minutes each on the same
host (about two of those minutes are systemd waiting for its random seed).

## The RV64 JIT (Orange Pi RV)

`riscv64/src/jit.rs` drives a separate JIT for the RISC-V core with three code generators: RV64 to
AArch64 (`jit_a64.rs`), RV64 to x86-64 on Linux (`jit_x64.rs`, which follows the AArch64 one
instruction for instruction) and RV64 to WebAssembly (`jit_wasm.rs`). Its regions are keyed by physical
address and shared by the four harts, stay valid until `fence.i` (then are compared with the bytes
they were compiled from), and probe a per-mode micro-TLB for loads and stores. Natively, regions
are linked through a per-hart jump cache and keep the guest registers they use most in host
registers. Four busy shell loops run 16 times faster than in the interpreter (about 2.2 G RISC-V
instructions per second on an M5). The design, its tests and timings are in
[orangepi-rv.md](orangepi-rv.md#the-jit).

## The RV32 JIT (ESP32-S31)

`riscv-rv32/src/jit.rs`. The RV32 core runs decoded blocks (`block.rs`) in every privilege mode
and compiles hot ones, in regions of the blocks their branches reach on the same virtual page, to
AArch64 (macOS, Linux), to x86-64 on Linux (`jit/x64.rs`, the AArch64 backend's code shape
instruction for instruction) and to WebAssembly in the browser. Integer, multiply/divide,
bit-manipulation and control-transfer instructions become host code; loads and stores call
helpers that do the interpreter's translation and bus access exactly, and everything else calls
the interpreter for one instruction. The block interpreter stays the reference:
`riscv-rv32/tests/blocks_differential.rs` runs random programs (self-modifying code, device
accesses, traps, interrupts) with compiled blocks and stepping, and compares the whole state.
`RV32_JIT=0` turns it off. The ESP32-S31's time goes mostly to its device models, so the JIT
changes less there than on the other boards ([esp32s31.md](esp32s31.md)).
