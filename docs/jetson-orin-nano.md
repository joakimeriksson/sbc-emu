# NVIDIA Jetson Orin Nano 4GB

`--board jetson-orin-nano-4gb`, experimental.

The Tegra234 target boots an arm64 Linux kernel directly using a generated
device tree. The bring-up configuration has 4 GiB of RAM, one Cortex-A78AE
core, a GICv3 distributor and redistributor, the ARM architectural timer, and a
serial console on Tegra234's SBSA UART at `0x031d0000`. The CLI supports the
serial runner, scripts, and GDB.

## Boot Alpine

```sh
cargo build --release -p sbc-emu
sh scripts/jetson-nano-fixture.sh cache/jetson-orin-nano orin
D=cache/jetson-orin-nano
target/release/sbc-emu run --board jetson-orin-nano-4gb \
  --kernel "$D/vmlinuz" --initrd "$D/initramfs.cpio.gz" \
  --script scripts/jetson-orin-nano-shell.txt --timeout 120
```

The fixture uses the checksummed Alpine packages shared with the Tegra210
fixture. For interactive use, omit `--script` and `--timeout`. Use
`--ram-mb 768` for a smaller interactive guest or `--no-jit` for the interpreter.
The scripted check expects the default 4 GiB configuration.

## Follow-on work

The next device milestone is six-core PSCI and the high-speed UART used by the
Waveshare rover. The generated device tree currently describes the devices
needed for direct boot and console I/O. Factory DTB and JetPack compatibility
will need the Tegra HSP/BPMP interfaces and storage controller. The board
currently has no NVMe, USB, network, GPU, or boot firmware model.

The hardware addresses and interrupt bindings come from the
[Linux Tegra234 device tree](https://github.com/torvalds/linux/blob/master/arch/arm64/boot/dts/nvidia/tegra234.dtsi).
