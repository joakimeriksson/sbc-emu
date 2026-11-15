#!/usr/bin/env python3
"""Create a disposable Orange Pi 5 SD-root boot fixture (requires mke2fs and cpio)."""
import argparse
import lzma
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

INITRD = '''#!/bin/sh
/bin/busybox --install -s /bin
mount -t devtmpfs devtmpfs /dev
mount -t proc proc /proc
mount -t sysfs sysfs /sys
for module in crc32c_generic crc16 mbcache jbd2 ext4 dw_mmc dw_mmc-pltfm dw_mmc-rockchip; do
    insmod /mod/$module.ko || { echo "FAILED module $module"; exec sh; }
done
for n in 1 2 3 4 5; do [ -b /dev/mmcblk0 ] && break; sleep 1; done
mount -t ext4 /dev/mmcblk0 /newroot || { echo SD_ROOT_MOUNT_FAILED; exec sh; }
exec switch_root /newroot /sbin/init
'''
ROOT_INIT = '''#!/bin/sh
/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
export PS1='sdroot# '
echo SD_ROOT_MOUNTED
exec setsid -c /bin/sh -l </dev/ttyS2 >/dev/ttyS2 2>&1
'''
MODULES = {
    'crc32c_generic': 'crypto/crc32c_generic',
    'crc16': 'lib/crc16', 'mbcache': 'fs/mbcache',
    'jbd2': 'fs/jbd2/jbd2', 'ext4': 'fs/ext4/ext4',
    'dw_mmc': 'drivers/mmc/host/dw_mmc',
    'dw_mmc-pltfm': 'drivers/mmc/host/dw_mmc-pltfm',
    'dw_mmc-rockchip': 'drivers/mmc/host/dw_mmc-rockchip',
}

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--busybox', type=Path, required=True, help='static AArch64 BusyBox')
    parser.add_argument('--modules', type=Path, required=True, help='kernel modules directory containing kernel/')
    parser.add_argument('--output', type=Path, required=True, help='new directory for scratch artifacts')
    parser.add_argument('--mke2fs', default='mke2fs', help='path to mke2fs')
    args = parser.parse_args()
    # Never overwrite a user's image or fixture directory.
    args.output.mkdir(parents=True, exist_ok=False)
    output = args.output.resolve()
    busybox = args.busybox.resolve()
    modules = args.modules.resolve() / 'kernel'
    image = output / 'sd-root.img'
    initramfs = output / 'initramfs.cpio'
    with tempfile.TemporaryDirectory(prefix='orangepi-root-', dir=output) as temp:
        base = Path(temp)
        root = base / 'root'
        early = base / 'initrd'
        for d in ['bin', 'sbin', 'dev', 'proc', 'sys', 'tmp']:
            (root / d).mkdir(parents=True, exist_ok=True)
        for d in ['bin', 'dev', 'proc', 'sys', 'newroot', 'mod']:
            (early / d).mkdir(parents=True, exist_ok=True)
        for target in [root, early]:
            shutil.copyfile(busybox, target / 'bin/busybox')
            (target / 'bin/busybox').chmod(0o755)
            (target / 'bin/sh').symlink_to('busybox')
        for name, relative in MODULES.items():
            source = modules / (relative + '.ko.xz')
            (early / 'mod' / (name + '.ko')).write_bytes(lzma.decompress(source.read_bytes()))
        for path, content in [(root / 'sbin/init', ROOT_INIT), (early / 'init', INITRD)]:
            path.write_text(content)
            path.chmod(0o755)
        with image.open('xb') as f:
            f.truncate(64 * 1024 * 1024)
        subprocess.run([args.mke2fs, '-q', '-F', '-t', 'ext4', '-d', str(root), str(image)], check=True)
        names = [os.path.relpath(p, early) for p in early.rglob('*')]
        with initramfs.open('xb') as f:
            subprocess.run(['cpio', '-o', '-H', 'newc', '--quiet'], cwd=early,
                           input=('\n'.join(sorted(names)) + '\n').encode(), stdout=f, check=True)
    print(f'SD image: {image}\nInitramfs: {initramfs}')

if __name__ == '__main__':
    main()
