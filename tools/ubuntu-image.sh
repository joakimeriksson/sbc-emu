#!/bin/sh
# Prepare a working copy of Ubuntu's Raspberry Pi image for the emulator (macOS):
# clone the pristine image, grow it to 16 GiB and seed cloud-init with a test account.
#   tools/ubuntu-image.sh cache/os/ubuntu-26.04.1-preinstalled-desktop-arm64+raspi.img cache/os/ubuntu-desktop.img
set -e
src=$1; dst=$2
[ -f "$src" ] && [ -n "$dst" ] || { echo "usage: $0 PRISTINE.img WORKING.img" >&2; exit 2; }
rm -f "$dst"
cp -c "$src" "$dst" 2>/dev/null || cp "$src" "$dst"
truncate -s 16g "$dst"
dev=$(diskutil image attach --noMount "$dst" | awk 'NR==1{print $1}')
diskutil mount -mountOptions nobrowse "${dev}s1" >/dev/null
mnt=$(diskutil info "${dev}s1" | awk -F': *' '/Mount Point/{print $2}')
cp "$(dirname "$0")/../scripts/ubuntu-user-data.yaml" "$mnt/user-data"
diskutil unmount "${dev}s1" >/dev/null
diskutil eject "$dev" >/dev/null
echo "$dst: seeded with scripts/ubuntu-user-data.yaml"
