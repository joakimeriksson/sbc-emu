# Per-pixel comparison of two directories of 128x128 RGBA8 dumps (*.rgba): GPU vs reference.
import glob, os, sys
W = H = 128
a, b = sys.argv[1], sys.argv[2]
fails = 0
for pa in sorted(glob.glob(os.path.join(a, '*.rgba'))):
    name = os.path.basename(pa)[:-5]
    pb = os.path.join(b, name + '.rgba')
    if not os.path.exists(pb): print(f'{name:20s} missing in {b}'); fails += 1; continue
    x, y = open(pa, 'rb').read(), open(pb, 'rb').read()
    off = maxd = 0; total = 0
    for p in range(0, W * H * 4, 4):
        d = max(abs(x[p + c] - y[p + c]) for c in range(4))
        maxd = max(maxd, d); total += d
        if d > 12: off += 1
    pct = 100.0 * off / (W * H)
    ok = pct < 1.0
    fails += not ok
    print(f'{name:20s} {"PASS" if ok else "FAIL"}  pixels off by >12: {pct:6.2f}%  max diff {maxd:3d}')
    with open(os.path.join(a, name + '.ppm'), 'wb') as f:
        f.write(b'P6 %d %d 255\n' % (W * 3, H))
        for row in range(H - 1, -1, -1):
            for img in (x, y, None):
                for col in range(W):
                    p = (row * W + col) * 4
                    if img is None: f.write(bytes(min(255, abs(x[p + c] - y[p + c]) * 4) for c in range(3)))
                    else: f.write(img[p:p + 3])
print(f'{fails} differ')
