#!/bin/sh
# Recompress an sbc-emu snapshot (SBCSNAP1 + deflate) as one zstd frame of its inflated body,
# for the browser build: the 3D demo snapshot shrinks from 82 MB (deflate at its best) to 63 MB,
# and unpacks in about a second. Only the WebAssembly build's sbc_restore reads this form. The
# frame records the unpacked size (so the input is a file, not a pipe) and keeps a 32 MiB window,
# so the browser needs little memory beyond the snapshot itself.
#   tools/snap-zstd.sh web/assets/gpu-demo.snap gpu-demo.snap.zst
set -eu
raw=$(mktemp)
trap 'rm -f "$raw"' EXIT
python3 -I -c 'import sys, zlib
d = open(sys.argv[1], "rb").read()
if d[:8] != b"SBCSNAP1": sys.exit("not an sbc-emu snapshot")
open(sys.argv[2], "wb").write(zlib.decompress(d[8:], -15))' "$1" "$raw"
zstd -q -f --ultra -22 --zstd=wlog=25 -T0 -o "$2" "$raw"
chmod 644 "$2"
ls -l "$2"
