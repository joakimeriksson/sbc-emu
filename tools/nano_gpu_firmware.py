#!/usr/bin/env python3
"""Placeholder firmware for the Jetson Nano's GPU (GM20B), for nouveau in the emulator.

The emulator does not run the GPU's microcontrollers: it answers for them. nouveau still has to find
files with valid headers. These have the headers and no code; NVIDIA's own (Debian's
firmware-nvidia-graphics) take their place when they are installed.

Usage: nano_gpu_firmware.py DIR     (writes DIR/nvidia/gm20b/...)
"""
import os, struct, sys

def u32(*v):
    return struct.pack('<%dI' % len(v), *v)

def bin_file(header, data):
    # struct nvfw_bin_hdr, the header, the data
    at = 24 + len(header)
    at += -at % 256
    body = header + bytes(at - 24 - len(header)) + data
    return u32(0x10de, 1, 24 + len(body), 24, at, len(data)) + body

def bootloader(tag):
    return bin_file(u32(tag, 0, 0, 256, 0, 0), bytes(256))  # struct nvfw_bl_desc

def secure_loader():
    # a header block: struct nvfw_hs_header, the two patch words, the load header, two signatures
    base = 24
    patch, load, sig = base + 32, base + 40, base + 80
    data_base = 0x200
    hs = u32(sig, 16, sig + 16, 16, patch, patch + 4, load, 28)
    header = hs + u32(data_base, 0) + u32(0, 0x100, data_base, 0x300, 1, 0x100, 0x100)
    header += bytes(80 - len(header)) + bytes(32)
    return bin_file(header, bytes(0x500))

def pmu_desc():
    # struct nvfw_ls_desc
    head = u32(132, 0x400, 0, 0) + bytes(64) + u32(0, 0x100, 0, 0, 0x100, 0x300, 0, 0, 0, 0, 0x200, 0x200, 0x100)
    return head + bytes(4 + 64 * 8 + 4)

FILES = {
    'acr/bl.bin': bootloader(0xbf),
    'acr/ucode_load.bin': secure_loader(),
    'pmu/desc.bin': pmu_desc(), 'pmu/image.bin': bytes(0x400), 'pmu/sig.bin': bytes(76),
    'gr/fecs_bl.bin': bootloader(0x3f), 'gr/fecs_inst.bin': bytes(256), 'gr/fecs_data.bin': bytes(256), 'gr/fecs_sig.bin': bytes(76),
    'gr/gpccs_inst.bin': bytes(256), 'gr/gpccs_data.bin': bytes(256),
    # register lists of one harmless entry: (address, index, data), (address, data)
    'gr/sw_ctx.bin': u32(0x404154, 0, 0), 'gr/sw_nonctx.bin': u32(0x40415c, 0),
    'gr/sw_bundle_init.bin': u32(0x4, 0), 'gr/sw_method_init.bin': u32((0x110 >> 2) << 16 | 0xb197, 0),
}

if __name__ == '__main__':
    for name, data in FILES.items():
        path = os.path.join(sys.argv[1], 'nvidia/gm20b', name)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, 'wb') as f:
            f.write(data)
