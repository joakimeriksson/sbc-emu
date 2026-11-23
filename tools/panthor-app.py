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
import shlex
import subprocess
import tempfile

root = pathlib.Path(__file__).resolve().parent.parent


def guest_program(app, frames, benchmarks):
    """The guest's command line and the serial output that shows the program started.

    Benchmark options are quoted for the guest's shell (a ' in one used to end the quoting):

    >>> program, started = guest_program('glmark2', 0, ["shading:shading=phong", "it's"])
    >>> print(program, started)
    glmark2-es2-drm -b shading:shading=phong -b 'it'"'"'s' [shading]
    >>> shlex.split(program)
    ['glmark2-es2-drm', '-b', 'shading:shading=phong', '-b', "it's"]
    >>> guest_program('kmscube', 240, [])
    ('kmscube -c 240', 'Rendered')
    >>> guest_program('glmark2', 0, ['a\\nb'])
    Traceback (most recent call last):
    ...
    ValueError: benchmark option with a backslash or control character: 'a\\nb'
    """
    if app == 'kmscube':
        return f'kmscube -c {frames}', 'Rendered'
    for b in benchmarks:
        # The emulator's script expands \n, \r, \t; the guest's shell reads a line.
        if '\\' in b or any(ord(c) < 32 or ord(c) == 127 for c in b):
            raise ValueError(f'benchmark option with a backslash or control character: {b!r}')
    # glmark2-es2-drm renders full screen at the largest mode the monitor offers.
    program = 'glmark2-es2-drm ' + ' '.join(f'-b {shlex.quote(b)}' for b in benchmarks)
    # glmark2 prints a scene's name when it starts drawing it (and its score after).
    first = benchmarks[0].split(':')[0] if benchmarks else 'build'
    return program.rstrip(), f'[{first}]'


def run(cmd, timeout):
    """Run the emulator: (exit status, or None when it did not finish in time; output; host
    diagnostics). What it printed before a timeout is kept.

    >>> run(['sh', '-c', 'echo partial; sleep 5'], 0.5)
    (None, 'partial\\n', '')
    >>> run(['sh', '-c', 'echo out; echo err >&2; exit 3'], 5)
    (3, 'out\\n', 'err\\n')
    """
    try:
        r = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout)
        status, out, err = r.returncode, r.stdout, r.stderr
    except subprocess.TimeoutExpired as e:
        status, out, err = None, e.stdout, e.stderr
    text = lambda b: (b or b'').decode(errors='replace')
    return status, text(out), text(err)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--image', type=pathlib.Path, required=True)
    p.add_argument('--app', choices=('kmscube', 'glmark2'), default='kmscube')
    p.add_argument('--frames', type=int, default=900, help='kmscube frame count')
    p.add_argument('--benchmark', action='append', default=[],
                   help='glmark2 benchmark (repeatable), e.g. build:use-vbo=true')
    p.add_argument('--hdmi-mode', default='640x480', help='preferred monitor mode: 640x480, 1280x720 or 1920x1080')
    p.add_argument('--screenshots', type=int, default=3, help='screenshots taken while the program runs')
    p.add_argument('--interval', type=float, default=2, help='seconds between screenshots')
    p.add_argument('--env', default='', help='extra guest environment, e.g. PAN_MESA_DEBUG=linear')
    p.add_argument('--output', type=pathlib.Path, default=root / 'out/kmscube.png',
                   help='first screenshot; the others get -2, -3, ... suffixes')
    p.add_argument('--emulator', type=pathlib.Path, default=root / 'target/release/sbc-emu')
    p.add_argument('--timeout', type=int, default=900)
    a = p.parse_args()
    try:
        program, started = guest_program(a.app, a.frames, a.benchmark)
    except ValueError as e:
        p.error(str(e))
    a.output.parent.mkdir(parents=True, exist_ok=True)
    shots = [a.output] + [a.output.with_name(f'{a.output.stem}-{n}{a.output.suffix}') for n in range(2, a.screenshots + 1)]
    setup = ('mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t tmpfs none /tmp; '
             'modprobe fixed; modprobe panthor; modprobe phy-rockchip-samsung-hdptx; modprobe rockchipdrm; '
             'sleep 2; stty -echo')
    lines = ['wait-serial root@', f'serial {setup}; echo APP-READY-$((6*7))\\n', 'wait-serial APP-READY-42',
             # The computed markers cannot match the shell's echo of the command.
             f'serial {a.env} {program} 2>&1; echo APP-EXIT-$((6*7))\\n',
             f'wait-serial {started}']
    for n, shot in enumerate(shots):
        if n or a.app == 'glmark2':
            lines.append(f'sleep {a.interval}')
        lines.append(f'screenshot {shot.resolve()}')
    lines += ['wait-serial APP-EXIT-42', 'quit']
    with tempfile.TemporaryDirectory(prefix='panthor-app-') as d:
        script = pathlib.Path(d) / 'script.txt'
        script.write_text('\n'.join(lines) + '\n')
        cmd = [str(a.emulator), 'run', '--board', 'orangepi5', '--gpu', 'probe', '--net', 'none',
               '--hdmi-mode', a.hdmi_mode, '--sd', str(a.image.resolve()), '--append', 'init=/bin/bash',
               '--script', str(script), '--timeout', str(a.timeout)]
        status, log, host = run(cmd, a.timeout + 60)
    logpath = a.output.with_suffix('.log')
    logpath.write_text(log + '\n[host diagnostics]\n' + host)
    errors = [l for l in (log + host).splitlines() if 'MESA: error' in l or 'queue stopped' in l or '[mali] firmware queue error' in l]
    if status is None:
        errors.insert(0, f'the emulator did not finish within {a.timeout + 60} s')
    for l in re.findall(r'^.*(?:Rendered \d+ frames|FPS: \d+|glmark2 Score: \d+).*$', log, re.M):
        print(l.strip())
    if status != 0 or 'APP-EXIT-42' not in log or errors or not a.output.exists():
        raise SystemExit(f'{a.app} failed; see {logpath}' + ''.join('\n  ' + e for e in errors[:5]))
    print(f'{a.app} ran on the emulated HDMI monitor; screenshots: ' + ', '.join(str(s) for s in shots if s.exists()))


if __name__ == '__main__':
    main()
