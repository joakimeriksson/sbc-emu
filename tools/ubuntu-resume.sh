#!/bin/sh
# Resume the logged-in Ubuntu Desktop snapshot (see scripts/ubuntu-desktop-snap.txt) and show it
# over VNC: `open vnc://127.0.0.1:5901`, password raspberry. Changes live in memory and are
# dropped on exit (the base image must stay exactly as it was when the snapshot was taken).
cd "$(dirname "$0")/.." || exit 1
exec ./target/release/sbc-emu run --sd cache/os/ubuntu-desktop-base.img --restore cache/os/ubuntu-desktop.snap \
    --ram-mb 3072 --threads multi --hdmi-mode 1280x720 --vnc ":1,password=raspberry" "$@"
