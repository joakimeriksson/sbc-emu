#!/usr/bin/env python3
"""Verify the Orin demo through the emulator's VNC protocol over pipes."""
import argparse
import os
from pathlib import Path
import select
import struct
import subprocess
import time
import zlib


def screenshot(path, width, height, frame):
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    rgb = bytearray(len(frame) // 4 * 3)
    rgb[0::3], rgb[1::3], rgb[2::3] = frame[2::4], frame[1::4], frame[0::4]
    stride = width * 3
    rows = b"".join(b"\0" + rgb[start:start + stride] for start in range(0, len(rgb), stride))
    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    path.write_bytes(
        b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b"")
    )


def verify(child, output):
    deadline = time.monotonic() + 15

    def read(length):
        data = bytearray()
        while len(data) < length:
            ready, _, _ = select.select([child.stdout], [], [], max(0, deadline - time.monotonic()))
            if not ready:
                raise TimeoutError("VNC update timed out")
            block = os.read(child.stdout.fileno(), length - len(data))
            if not block:
                raise EOFError("VNC server closed")
            data.extend(block)
        return data

    def send(data):
        child.stdin.write(data)
        child.stdin.flush()

    assert read(12) == b"RFB 003.008\n"
    send(b"RFB 003.008\n")
    assert read(2) == b"\x01\x01"
    send(b"\x01")
    assert read(4) == b"\0" * 4
    send(b"\x01")
    width, height = struct.unpack(">HH", read(4))
    assert (width, height) == (640, 360)
    pixel_format = read(16)
    assert pixel_format[:4] == bytes([32, 24, 0, 1])
    assert pixel_format[10:13] == bytes([16, 8, 0])
    name_length, = struct.unpack(">I", read(4))
    assert read(name_length) == b"sbc-emu: Jetson Orin Nano"
    frame = bytearray(width * height * 4)

    def update(incremental):
        send(struct.pack(">BBHHHH", 3, incremental, 0, 0, width, height))
        kind, _, rectangles = struct.unpack(">BBH", read(4))
        assert kind == 0 and rectangles > 0
        for _ in range(rectangles):
            x, y, w, h, encoding = struct.unpack(">HHHHi", read(12))
            assert encoding == 0 and x + w <= width and y + h <= height
            pixels = read(w * h * 4)
            for row in range(h):
                left = ((y + row) * width + x) * 4
                frame[left:left + w * 4] = pixels[row * w * 4:(row + 1) * w * 4]

    def pixel(x, y):
        i = (y * width + x) * 4
        return frame[i:i + 3]

    def rover_x():
        return next((x for x in range(width) if pixel(x, 250) == b"\xae\xba\x30"), None)

    update(False)
    while rover_x() is None:
        update(True)
    first_position = rover_x()
    assert pixel(493, 94) == b"\x70\xad\xf3", "wrong terrain or pixel order"
    update(True)
    assert rover_x() is not None and rover_x() != first_position, "rover did not move"
    if output:
        screenshot(output, width, height, frame)
    print(f"VNC {width}x{height}, rover moved from x={first_position} to x={rover_x()}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--emulator", default="target/release/sbc-emu")
    parser.add_argument("--image", default="cache/jetson-orin-nano/copy-demo.Image")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    command = [
        args.emulator, "run", "--board", "jetson-orin-nano-4gb", "--smp", "1",
        "--ram-mb", "256", "--gpu", "on", "--kernel", args.image, "--vnc", "stdio", "--timeout", "15",
    ]
    with subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE) as child:
        try:
            verify(child, args.output)
            child.stdin.close()
            if child.wait(timeout=3) != 0:
                raise RuntimeError(f"emulator exited with status {child.returncode}")
        finally:
            if child.poll() is None:
                child.kill()


if __name__ == "__main__":
    main()
