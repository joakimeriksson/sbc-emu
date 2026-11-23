#!/usr/bin/env python3
"""Run a real KMS/GLES program (kmscube, glmark2-es2-drm) on the emulated Orange Pi 5.

The program draws through Mesa's Panfrost driver on the emulated Mali-G610 and shows
its frames on the emulated HDMI monitor (rockchipdrm, VOP2). Screenshots are taken from
the monitor while the program runs; the guest log is kept next to them. The image must
have the program installed (see docs/orangepi5-gpu.md); it is opened copy-on-write.
"""
import argparse
import pathlib
import re
import subprocess
import tempfile

root = pathlib.Path(__file__).resolve().parent.parent
p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--image', type=pathlib.Path, required=True)
p.add_argument('--app', choices=('kmscube', 'glmark2'), default='kmscube')
p.add_argument('--frames', type=int, default=900, help='kmscube frame count')
p.add_argument('--benchmark', action='append', default=[],
               help='glmark2 benchmark (repeatable), e.g. build:use-vbo=true')
p.add_argument('--hdmi-mode', default='640x480', help='monitor mode: 640x480, 1280x720 or 1920x1080')
p.add_argument('--screenshots', type=int, default=3, help='screenshots taken while the program runs')
p.add_argument('--interval', type=float, default=2, help='seconds between screenshots')
p.add_argument('--env', default='', help='extra guest environment, e.g. PAN_MESA_DEBUG=linear')
p.add_argument('--output', type=pathlib.Path, default=root / 'out/kmscube.png',
               help='first screenshot; the others get -2, -3, ... suffixes')
p.add_argument('--emulator', type=pathlib.Path, default=root / 'target/release/sbc-emu')
p.add_argument('--timeout', type=int, default=900)
a = p.parse_args()

a.output.parent.mkdir(parents=True, exist_ok=True)
shots = [a.output] + [a.output.with_name(f'{a.output.stem}-{n}{a.output.suffix}') for n in range(2, a.screenshots + 1)]
if a.app == 'kmscube':
    program = f'kmscube -c {a.frames}'
    started = 'Rendered'
else:
    benchmarks = ' '.join(f"-b '{b}'" for b in a.benchmark)
    program = f'glmark2-es2-drm {benchmarks}'
    started = '======================================================='
setup = ('mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t tmpfs none /tmp; '
         'modprobe fixed; modprobe panthor; modprobe phy-rockchip-samsung-hdptx; modprobe rockchipdrm; '
         'sleep 2; stty -echo')
lines = ['wait-serial root@', f'serial {setup}; echo APP-READY-$((6*7))\\n', 'wait-serial APP-READY-42',
         # The computed markers cannot match the shell's echo of the command.
         f'serial {a.env} {program} 2>&1; echo APP-EXIT-$((6*7))\\n',
         f'wait-serial {started}']
for n, shot in enumerate(shots):
    if n:
        lines.append(f'sleep {a.interval}')
    lines.append(f'screenshot {shot.resolve()}')
lines += ['wait-serial APP-EXIT-42', 'quit']
with tempfile.TemporaryDirectory(prefix='panthor-app-') as d:
    script = pathlib.Path(d) / 'script.txt'
    script.write_text('\n'.join(lines) + '\n')
    cmd = [str(a.emulator), 'run', '--board', 'orangepi5', '--gpu', 'probe', '--net', 'none',
           '--hdmi-mode', a.hdmi_mode, '--sd', str(a.image.resolve()), '--append', 'init=/bin/bash',
           '--script', str(script), '--timeout', str(a.timeout)]
    result = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=a.timeout + 60)
log = result.stdout.decode(errors='replace')
host = result.stderr.decode(errors='replace')
logpath = a.output.with_suffix('.log')
logpath.write_text(log + '\n[host diagnostics]\n' + host)
errors = [l for l in (log + host).splitlines() if 'MESA: error' in l or 'queue stopped' in l or '[mali] firmware queue error' in l]
for l in re.findall(r'^.*(?:Rendered \d+ frames|FPS: \d+|glmark2 Score: \d+).*$', log, re.M):
    print(l.strip())
if result.returncode or 'APP-EXIT-42' not in log or errors or not a.output.exists():
    raise SystemExit(f'{a.app} failed; see {logpath}' + ''.join('\n  ' + e for e in errors[:5]))
print(f'{a.app} ran on the emulated HDMI monitor; screenshots: ' + ', '.join(str(s) for s in shots if s.exists()))
