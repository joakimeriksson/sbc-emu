#!/usr/bin/env python3
"""Run the Linux Panthor clear-pass demo and extract its PNG from serial output."""
import argparse, base64, pathlib, subprocess, tempfile, zlib, re
root=pathlib.Path(__file__).resolve().parent.parent
p=argparse.ArgumentParser(description=__doc__)
mode=p.add_mutually_exclusive_group()
mode.add_argument('--shader',action='store_true',help='Run the compute-shader triangle demo')
mode.add_argument('--egl',action='store_true',help='Verify a real Mesa GLES clear (image needs Mesa EGL/GLES/GBM packages)')
p.add_argument('--image',type=pathlib.Path,required=True)
p.add_argument('--emulator',type=pathlib.Path,default=root/'target/release/sbc-emu')
p.add_argument('--output',type=pathlib.Path,default=root/'out/panthor-clear-demo.png')
p.add_argument('--timeout',type=int,default=180)
a=p.parse_args()
source=root/('tests/gpu/panthor-egl.py' if a.egl else 'tests/gpu/panthor-shader.py' if a.shader else 'tests/gpu/panthor-clear.py')
marker='PANTHOR_EGL_CLEAR_OK' if a.egl else 'PANTHOR_SHADER_OK' if a.shader else 'PANTHOR_CLEAR_OK'
payload=base64.b64encode(zlib.compress(source.read_bytes())).decode()
lines=['wait-serial root@','serial mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t tmpfs none /tmp; modprobe fixed; modprobe panthor; stty -echo\\n','serial : > /tmp/demo.b64\\n']
for i in range(0,len(payload),500):lines.append('serial printf %s '+payload[i:i+500]+' >> /tmp/demo.b64\\n')
command = 'serial python3 -c \'import base64,zlib;exec(zlib.decompress(base64.b64decode(open("/tmp/demo.b64").read())))\''
if a.egl:
    # Finish and preserve diagnostics even when the guest test fails. A computed
    # marker avoids matching the shell's echoed command before it executes.
    lines += [command + '; echo PANTHOR-DONE-$((6*7))\\n', 'wait-serial PANTHOR-DONE-42', 'quit']
else:
    lines += [command + '\\n', 'wait-serial ' + marker, 'wait-serial IMAGE_END', 'quit']
with tempfile.TemporaryDirectory(prefix='panthor-demo-') as d:
    script=pathlib.Path(d)/'script.txt';script.write_text('\n'.join(lines)+'\n')
    cmd=[str(a.emulator),'run','--board','orangepi5','--gpu','probe','--net','none','--sd',str(a.image.resolve()),'--append','init=/bin/bash','--script',str(script),'--timeout',str(a.timeout)]
    result=subprocess.run(cmd,stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=a.timeout+30)
    log=result.stdout.decode(errors='replace')
    a.output.parent.mkdir(parents=True,exist_ok=True)
    logpath=a.output.with_suffix('.log');logpath.write_text(log+'\n[host diagnostics]\n'+result.stderr.decode(errors='replace'))
    if a.egl:
        if result.returncode or marker not in log:
            raise SystemExit('Mesa GLES clear failed; see '+str(logpath))
        print('Mesa GLES clear verified through Panthor; log: '+str(logpath))
        raise SystemExit(0)
    match=re.search(r'PNG_BASE64=([A-Za-z0-9+/=]+)',log)
    if result.returncode or not match or marker not in log or 'IMAGE_END' not in log:
        raise SystemExit('Demo failed; see '+str(logpath))
    png=base64.b64decode(match[1],validate=True)
    if not png.startswith(b'\x89PNG\r\n\x1a\n'):raise SystemExit('Invalid PNG output')
    a.output.write_bytes(png)
    print('Linux Panthor submission and fence completed; image: '+str(a.output))
