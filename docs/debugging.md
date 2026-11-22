# Debugging a guest with gdb or lldb

Every board has a GDB remote stub:

```sh
sbc-emu run --board BOARD ... --gdb 1234              # attach at any time
sbc-emu run --board BOARD ... --gdb 1234 --gdb-wait   # hold the machine before its first instruction
```

Then `target remote :1234` in gdb (`gdb-multiarch`), or `gdb-remote 1234` in lldb. The stub
describes the registers itself, so no architecture has to be set; load the guest's symbols with
`file vmlinux`.

| | |
|---|---|
| Threads | One per core (`info threads` shows its state and mode). `stepi` steps one core with the others held. |
| Registers | Integer, program counter, status and floating point, read and write. A32: not the mode bits of `cpsr`. |
| Memory | Virtual addresses as the selected core sees them, through its page tables and without side effects. RAM only, plus ROM and flash for reading on the ESP32-S31; device registers are not read. |
| Breakpoints | `break` and `hbreak`, any number, virtual addresses on every core. |
| Watchpoints | `watch`, `rwatch`, `awatch`, any length. The stop comes after the access. |
| Stops | Ctrl-C; instructions the emulator does not implement stop in the debugger instead of ending the run. |
| Reset | Breakpoints and watchpoints stay set across a guest reboot. |

While a breakpoint or watchpoint is set the cores run in the interpreter (and one host thread), so
the guest is slower; the ESP32-S31 keeps its JIT for breakpoints. Detaching, or the debugger going
away, clears them and the JIT comes back. With none set there is nothing to check on the way.
Guest time stands still while the machine is halted.

Not there: the browser pages have no debugger; no system registers (`CP15`, `CSR`s) beyond those
above; no reverse execution.

`tools/gdb-check.py [BOARD ...]` boots each board's guest and drives all of this over the
protocol.
