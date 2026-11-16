#!/usr/bin/env python3
"""Write an emulator script that shows pages in Firefox and/or Chromium under cage on Raspberry Pi OS
(the image made by scripts/raspios-browser-setup.txt), screenshots each one and prints the
browser's GL messages and the kernel's v3d messages.

   tools/browser-script.py OUT.txt SHOTDIR browser:page:seconds ...
       browser = ff | cr, or ffsw | crsw for the browser on llvmpipe as a reference
       page    = a path under tests/gpu (served by tools/gpu_server.py 8800) or a full URL

   e.g. tools/browser-script.py out/br.txt out/br ff:web/cubes.html:120 cr:chars.html:90
        sbc-emu run --sd cache/os/raspios-browser.img --threads multi --no-display --script out/br.txt"""
import sys

out, shots, runs = sys.argv[1], sys.argv[2], sys.argv[3:]
H = 'http://10.0.2.2:8800'
USERJS = ';'.join(f"echo 'user_pref(\"{k}\", {v});' >> /tmp/ff/user.js" for k, v in [
    ('browser.shell.checkDefaultBrowser', 'false'), ('browser.aboutwelcome.enabled', 'false'),
    ('datareporting.policy.dataSubmissionEnabled', 'false'), ('toolkit.telemetry.reportingpolicy.firstRun', 'false'),
    ('browser.startup.homepage_override.mstone', '"ignore"'), ('gfx.webrender.software', 'false'),
    ('layers.acceleration.force-enabled', 'true'), ('webgl.force-enabled', 'true'),
    ('devtools.console.stdout.content', 'true'), ('browser.sessionstore.resume_from_crash', 'false')])
L = ['wait-serial raspberrypi login:', 'sleep 2', 'serial pi\\n', 'sleep 2', 'wait-idle 4', 'serial raspberry\\n', 'sleep 3', 'wait-idle 6',
     'serial echo raspberry | sudo -S true; (sudo seatd -g video > /tmp/seatd.log 2>&1 &); sleep 2; export XDG_RUNTIME_DIR=/run/user/1000 LIBSEAT_BACKEND=seatd;'
     f' rm -rf /tmp/ff; mkdir -p /tmp/ff; {USERJS}; echo SET-""UP\\n', 'wait-serial SET-UP']
for k, r in enumerate(runs):
    b, page, secs = r.split(':')
    url = page if page.startswith('http') else f'{H}/{page}'
    name = f'{k:02d}-{b}-{page.split("/")[-1].split(".")[0]}'
    sw = b.endswith('sw')  # the browser alone on llvmpipe, as a reference (cage stays on V3D)
    cmd = ('env LIBGL_ALWAYS_SOFTWARE=1 ' if sw else '') + (f'firefox --kiosk --profile /tmp/ff {url}' if b.startswith('ff') else
           f'chromium --ozone-platform=wayland --kiosk --no-first-run --ignore-gpu-blocklist --enable-logging=stderr --user-data-dir=/tmp/cr {url}')
    L += [f'serial (cage -- {cmd} > /tmp/b.log 2>&1 &); echo GO-""{k}\\n', f'wait-serial GO-{k}', f'sleep {secs}',
          f'shot {shots}/{name}.png',
          f'serial pkill -f "cage --"; for i in $(seq 40); do pgrep -f "cage|firefox|chromium" > /dev/null || break; sleep 1; done; pkill -9 -f "firefox|chromium"; sleep 1; echo "== {name}"; grep -a -E "GLDEMO|CONSOLE|GPU|gpu_|EGL|error|Error|WebGL|WebRender|crash" /tmp/b.log | grep -av -i -E "dbus|vaapi|sandbox" | cut -c1-220 | sort | uniq -c | sort -rn | head -12; echo raspberry | sudo -S dmesg -c 2>/dev/null | grep -a -i -E "v3d|mmu|drm" | tail -6; echo LOG-""{k}\\n',
          f'wait-serial LOG-{k}']
L += ['sleep 2', 'quit']
open(out, 'w').write('\n'.join(L) + '\n')
