#!/usr/bin/env python3
"""Check Pi adapter boot, serial and reset in both execution modes.

Uses read-only direct-boot fixtures and writes logs only to --output. The script
ends in quit (exit status 0); the CLI also exits 0 when --timeout runs out with no
script left, so guest/script milestones are checked independently.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/sbc-emu"))
    parser.add_argument("--kernel", type=Path, required=True, help="uncompressed arm64 Image")
    parser.add_argument("--initrd", type=Path, required=True, help="existing busybox initramfs")
    parser.add_argument("--output", type=Path, default=Path("out/multi-board-smoke"))
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    script = Path(__file__).resolve().parent.parent / "scripts/multi-board-smoke.txt"
    report = {"fixtures": {}, "runs": []}
    for name, path in [("kernel", args.kernel), ("initrd", args.initrd), ("script", script), ("binary", args.binary)]:
        report["fixtures"][name] = {
            "path": str(path.resolve()),
            "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
        }
    for mode in ["single", "multi"]:
        command = [str(args.binary.resolve()), "run"]
        # Exercise the implicit default as well as explicit Pi selection.
        if mode == "multi":
            command += ["--board", "pi4b"]
        command += [
            "--kernel", str(args.kernel.resolve()), "--initrd", str(args.initrd.resolve()),
            "--ram-mb", "256", "--smp", "4", "--net", "none", "--no-display",
            "--display", "fb", "--no-bluetooth", "--threads", mode, "--seed", "42",
            "--script", str(script), "--timeout", "30",
        ]
        result = subprocess.run(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT, timeout=40)
        log = result.stdout.decode(errors="replace")
        (args.output / f"{mode}.log").write_bytes(result.stdout)
        before, separator, after = log.partition("[sbc-emu] system reset: rebooting")
        passed = (result.returncode == 0 and bool(separator)
                  and "wait-serial MULTI_READY" in before
                  and "sbc-emu: busybox shell." in before
                  and "sbc-emu: busybox shell." in after
                  and "wait-serial RESET_READY" in after)
        report["runs"].append({"mode": mode, "command": command,
                               "returncode": result.returncode, "passed": passed})
        (args.output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        if not passed:
            raise SystemExit(f"{mode} boot/serial/reset check failed: {args.output / (mode + '.log')}")
        print(f"{mode}: boot, serial input and reset passed")


if __name__ == "__main__":
    main()
