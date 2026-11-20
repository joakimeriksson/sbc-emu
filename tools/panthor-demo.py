#!/usr/bin/env python3
"""Run a Linux Panthor demo and extract its PNG from serial output."""
import argparse, base64, pathlib, subprocess, tempfile, zlib, re
root=pathlib.Path(__file__).resolve().parent.parent
p=argparse.ArgumentParser(description=__doc__)
mode=p.add_mutually_exclusive_group()
mode.add_argument('--shader',action='store_true',help='Run the compute-shader triangle demo')
mode.add_argument('--gles',action='store_true',help='Run the real Mesa GLES rendering demo (image needs Mesa libraries)')
mode.add_argument('--egl',action='store_true',help='Verify a real Mesa GLES clear (image needs Mesa EGL/GLES/GBM packages)')
mode.add_argument('--kms',action='store_true',help='Render the GLES scene on the HDMI screen (rockchipdrm/VOP2 scanout); the PNG is the emulated monitor')
p.add_argument('--shader-registers',type=int,choices=(32,64),default=64,help='Shader register allocation (requires --shader)')
p.add_argument('--image',type=pathlib.Path,required=True)
p.add_argument('--emulator',type=pathlib.Path,default=root/'target/release/sbc-emu')
p.add_argument('--output',type=pathlib.Path,default=root/'out/panthor-clear-demo.png')
p.add_argument('--timeout',type=int,default=180)
a=p.parse_args()
if a.shader_registers!=64 and not a.shader:p.error("--shader-registers requires --shader")
source=root/('tests/gpu/panthor-gles-kms.py' if a.kms else 'tests/gpu/panthor-gles-demo.py' if a.gles else 'tests/gpu/panthor-egl.py' if a.egl else 'tests/gpu/panthor-shader.py' if a.shader else 'tests/gpu/panthor-clear.py')
marker='PANTHOR_KMS_OK' if a.kms else 'PANTHOR_GLES_DEMO_OK' if a.gles else 'PANTHOR_EGL_CLEAR_OK' if a.egl else 'PANTHOR_SHADER_OK' if a.shader else 'PANTHOR_CLEAR_OK'
source_bytes=source.read_bytes()
if a.shader:
    source_bytes=('import os\nos.environ["PANTHOR_SHADER_REGISTERS"]="%d"\n'%a.shader_registers).encode()+source_bytes
payload=base64.b64encode(zlib.compress(source_bytes)).decode()
display='; modprobe phy-rockchip-samsung-hdptx; modprobe rockchipdrm; sleep 2' if a.kms else ''
lines=['wait-serial root@','serial mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t tmpfs none /tmp; modprobe fixed; modprobe panthor'+display+'; stty -echo\\n','serial : > /tmp/demo.b64\\n']
for i in range(0,len(payload),500):lines.append('serial printf %s '+payload[i:i+500]+' >> /tmp/demo.b64\\n')
command = 'serial python3 -c \'import base64,zlib;exec(zlib.decompress(base64.b64decode(open("/tmp/demo.b64").read())))\''
if a.kms:
    # the picture is taken from the emulated HDMI monitor while the guest holds it on screen
    lines += [command.replace('serial ','serial KMS_HOLD=3 ',1) + '; echo PANTHOR-DONE-$((6*7))\\n', 'wait-serial ' + marker, 'sleep 1', 'screenshot ' + str(a.output.resolve()), 'wait-serial PANTHOR-DONE-42', 'quit']
elif a.egl:
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
    if a.kms:
        if result.returncode or marker not in log or not a.output.exists():
            raise SystemExit('KMS demo failed; see '+str(logpath))
        print('Mesa GLES rendered on the HDMI screen (rockchipdrm/VOP2); screenshot: '+str(a.output))
        raise SystemExit(0)
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
    print(('Mesa GLES rendering completed; image: ' if a.gles else 'Linux Panthor submission and fence completed; image: ')+str(a.output))
