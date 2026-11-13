# Decisions

## D1 (2026-11-13): own Rust AArch64 core, no QEMU

SBC-EMUPLAN.md binding rules 1 and 7 (reuse QEMU, keep QEMU patches upstreamable) are
**superseded**. sbc-emu follows the architecture of esp32sim: an instruction-level emulator in
Rust with its own CPU core (`arm-a64`), register-level SoC models, one `Machine` per board and
deterministic virtual time. Reasons: one codebase style with esp32sim and Cooja-NG, exact
lock-step (`--cooja`, protocol v1 from csim `docs/design/external-nodes-plan.md` §4) without a
patched QEMU scheduler, and a future WebAssembly build.

Everything else in the plan stands: board priority (Pi 4B, Pi 3B first), truthful B0–B4
capability labels, evidence/report rules, W0–W3 and T0–T2 tiers. T1/T2 become properties of the
design rather than research gates, but still must be demonstrated by tests before being claimed.

Consequences: the core must implement ARMv8.0-A as on Cortex-A72/A53 (EL0–EL3, AArch64 only at
first, stage-1 MMU, generic timer, GIC-400 / BCM2836 local interrupt controller, FP/SIMD). ID
registers advertise exactly what is implemented (no LSE, no PAC, no SVE).

## D2 (2026-11-13): reduced device tree, emulated firmware boot

The emulator generates a reduced BCM2711 device tree describing only modelled devices (CPU,
GIC-400, generic timer, PL011, mailbox + firmware node, EMMC2, DWC2 in host mode, simplefb)
with vendor compatible strings and addresses. It does the firmware's job directly: it reads
`config.txt` ([all]/[pi4] sections), `cmdline.txt` (resolving `serial0`), `kernel8.img` (gunzip)
and the auto initramfs from the SD card's FAT partition. Reports call this `boot_path =
emulated-firmware`. The vendor DTB is not used because it references many unmodelled blocks
(PCIe, GENET, VC4/HVS, RP1-less clocks via firmware); adopting it is a later fidelity milestone.

## D3 (2026-11-13): keyboard through the DWC2 OTG port

The Raspberry Pi kernel has no virtio and no PS/2; real Pi 4 keyboards sit on the VL805 xHCI
behind PCIe. The smallest real-hardware path is the USB-C OTG controller (DWC2) in host mode, as
`dtoverlay=dwc2,dr_mode=host` configures it on a real board. The model implements the host side
in buffer-DMA mode plus a HID boot keyboard; Linux's unmodified `dwc2` and `usbhid` drive it.
VL805/xHCI remains the route for full Pi 4 USB fidelity.

## D4 (2026-11-13): whole-machine snapshots

A snapshot holds CPU state, all device state, virtual time, the SD overlay and non-zero RAM pages
(deflate). It exists so a slow cold boot (≈27 G instructions to the Raspberry Pi OS login) can be
demonstrated in the browser in seconds; a restored run is labelled as such. Wall-clock pacing is
re-anchored on restore so guest time continues monotonically.
