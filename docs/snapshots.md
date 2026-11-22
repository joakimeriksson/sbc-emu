# Snapshots

A snapshot is the whole machine in a file: cores, devices, virtual time, the RAM pages that are
not zero and what the guest has written to its disks. Restored, the guest goes on where it was.

| Board | |
|---|---|
| Raspberry Pi 4 | yes (script `save`, monitor `savevm`/`loadvm`, `--restore`) |
| Orange Pi 5, Orange Pi One, Tinker Board | yes (below) |
| The other boards | not yet |

```sh
sbc-emu run --board orangepi5 ... --script s.txt             # a script's `save FILE`
sbc-emu run --board orangepi5 ... --snapshot here.snap       # Ctrl-A s writes here.snap (default BOARD.snap)
sbc-emu run --board orangepi5 ... --restore here.snap        # go on from it
```

Restore with the options the snapshot was made with: the same `--smp`, `--ram-mb` (its default
depends on `--kernel`), `--net`, `--gpu`
and disk **images** (`--sd`, `--emmc`, `--nvme`, `--usb-drive`; a snapshot holds only what was written over
them). Cores, RAM, devices, and the images' sizes are checked, their contents are not. The kernel is
in the snapshot's RAM; `--kernel` is only needed if the guest is to reboot. `--no-jit` and
`--threads` may differ.

On one thread and interpreted (`--no-jit`), a restored run is the uninterrupted one, to the
instruction and byte; with the JIT or `--threads multi` it is as repeatable as those are.

Input typed but not yet taken by the guest is handed over first (for two seconds, if it takes none). `--hdmi-mode` is the snapshot's until
the guest reboots.

Not in a snapshot: connections through NAT (the guest's end hears nothing more, as if the cable had
been cut; new ones work), compiled code, audio recorded
so far (`--audio-out`). Refused: `--sd-persist`, which writes into the image, both to save and to restore with; a file that is
damaged. (On the Pi 4, whose format is older and has no checksum, a snapshot made with `--sd-persist`
is accepted, as long as the image is as it was then; one with written sectors is not restored onto
it.) A snapshot fits the
build that made it: when the layout changes, its version does, and another is refused (the Pi 4's
older format keeps its own compatibility).

## Adding a board

`emu_core::snap::Snap` is implemented with `snap_struct!`, which names every field of a structure as
saved, `fixed` (a length the hardware gives), `fitted` (there or not, as the machine is built) or `skip`: new state that is in none of them does not
compile. `rk3588/src/snapshot.rs` puts a machine together, and `rk3588/tests/snapshot.rs` is the test
to copy: run, save, go on; restore into a new machine, go on as far; the two states are equal.
