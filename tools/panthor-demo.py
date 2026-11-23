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
mode.add_argument('--planes',action='store_true',help='Show XRGB and ARGB (pre-multiplied, coverage) KMS overlay planes on the HDMI screen (VOP2 mixers, no GPU); the PNG is the emulated monitor')
p.add_argument('--shader-registers',type=int,choices=(32,64),default=64,help='Shader register allocation (requires --shader)')
p.add_argument('--image',type=pathlib.Path)
p.add_argument('--emulator',type=pathlib.Path,default=root/'target/release/sbc-emu')
p.add_argument('--output',type=pathlib.Path,default=root/'out/panthor-clear-demo.png')
p.add_argument('--timeout',type=int,default=180)
p.add_argument('--bottom-alpha',action='store_true',help='With --planes: the picture on an Esmart plane at zpos 0 with half plane alpha, the primary off (KMS_SCENE=bottom-alpha)')
p.add_argument('--check-planes',type=pathlib.Path,metavar='PNG',help='Only check the pixels of a --planes screenshot already taken (no emulator run)')
a=p.parse_args()

def read_png(path):
    """(width, height, rows of RGBA bytes) of an 8-bit RGBA, non-interlaced PNG."""
    data=path.read_bytes();pos=8;idat=b'';w=h=0
    while pos<len(data):
        n=int.from_bytes(data[pos:pos+4],'big');ty=data[pos+4:pos+8];body=data[pos+8:pos+8+n];pos+=12+n
        if ty==b'IHDR':
            w,h=int.from_bytes(body[0:4],'big'),int.from_bytes(body[4:8],'big')
            if body[8:13]!=bytes([8,6,0,0,0]):raise SystemExit('%s: not an 8-bit RGBA PNG'%path)
        elif ty==b'IDAT':idat+=body
    raw=zlib.decompress(idat);stride=4*w;rows=[];prev=bytearray(stride)
    for y in range(h):
        f=raw[y*(stride+1)];line=bytearray(raw[y*(stride+1)+1:(y+1)*(stride+1)])
        for i in range(stride):
            left=line[i-4] if i>=4 else 0;up=prev[i];ul=prev[i-4] if i>=4 else 0
            if f==1:line[i]=(line[i]+left)&255
            elif f==2:line[i]=(line[i]+up)&255
            elif f==3:line[i]=(line[i]+(left+up)//2)&255
            elif f==4:
                pa,pb,pc=abs(up-ul),abs(left-ul),abs(left+up-2*ul)
                line[i]=(line[i]+(left if pa<=pb and pa<=pc else up if pb<=pc else ul))&255
        rows.append(line);prev=line
    return w,h,rows

def check_planes(png,bottom_alpha=False):
    """Sample the patches of tests/gpu/rockchip-kms-planes.py against its docstring values
    (planes A, B, C over the white and the grey half, and the primary around them; with
    bottom_alpha, B over the bottom plane at half plane alpha, and that plane elsewhere)."""
    W,H,rows=read_png(png)
    pw,ph,py=W//4,H//3,H//2-H//6
    top,bottom=py+ph//4,py+3*ph//4
    def at(x,y):return tuple(rows[y][4*x:4*x+3])
    if bottom_alpha:
        checks=[('bottom plane, white half',W//2,H//8,(128,)*3),('bottom plane, grey half',W//2,H-H//12,(64,)*3),
                ('bottom plane, left',W//16,H//2-10,(128,)*3),('B over white x 1/2',W//2,top,(128,)*3),('B over grey x 1/2',W//2,bottom,(96,)*3)]
        planes=()
    else:
        checks=[('primary, white half',W//2,H//8,(255,255,255)),('primary, grey half',W//2,H-H//12,(128,128,128))]
        planes=(('A XRGB X=0',W//16,(255,0,0),(255,0,0)),('B Pre-multiplied',W//2-pw//2,(191,)*3,(128,)*3),('C Coverage',W-W//16-pw,(159,)*3,(96,)*3))
    for name,x,over_white,over_grey in planes:
        checks+=[(name+' over white',x+pw//2,top,over_white),(name+' over grey',x+pw//2,bottom,over_grey)]
    bad=0
    for name,x,y,want in checks:
        # a 5x5 patch, every pixel within 2 of the value
        got=[at(x+dx,y+dy) for dx in range(-2,3) for dy in range(-2,3)]
        ok=all(abs(c-v)<=2 for px in got for c,v in zip(px,want))
        bad+=not ok
        print('%-30s (%4d,%4d): %-15s want %-15s %s'%(name,x,y,got[12],want,'ok' if ok else 'WRONG'))
    # D: XRGB red, X byte 0, on a Cluster overlay over grey: what the mixers make of the X
    # byte (issue #54); mainline Linux's registers give red added to grey, 255,128,128
    if not bottom_alpha:print('%-30s (%4d,%4d): %s (not checked, issue #54)'%('D XRGB X=0 on a Cluster',W//16+pw//2,H-H//12-20,at(W//16+pw//2,H-H//12-20)))
    if bad:raise SystemExit('%d plane pixel checks failed in %s'%(bad,png))
    print('plane pixels match the expected blend values')

if a.check_planes:
    check_planes(a.check_planes,a.bottom_alpha);raise SystemExit(0)
if a.shader_registers!=64 and not a.shader:p.error("--shader-registers requires --shader")
if not a.image:p.error('--image is required')
if a.bottom_alpha and not a.planes:p.error('--bottom-alpha requires --planes')
source=root/('tests/gpu/rockchip-kms-planes.py' if a.planes else 'tests/gpu/panthor-gles-kms.py' if a.kms else 'tests/gpu/panthor-gles-demo.py' if a.gles else 'tests/gpu/panthor-egl.py' if a.egl else 'tests/gpu/panthor-shader.py' if a.shader else 'tests/gpu/panthor-clear.py')
marker='KMS_PLANES_OK' if a.planes else 'PANTHOR_KMS_OK' if a.kms else 'PANTHOR_GLES_DEMO_OK' if a.gles else 'PANTHOR_EGL_CLEAR_OK' if a.egl else 'PANTHOR_SHADER_OK' if a.shader else 'PANTHOR_CLEAR_OK'
source_bytes=source.read_bytes()
if a.shader:
    source_bytes=('import os\nos.environ["PANTHOR_SHADER_REGISTERS"]="%d"\n'%a.shader_registers).encode()+source_bytes
payload=base64.b64encode(zlib.compress(source_bytes)).decode()
display='; modprobe phy-rockchip-samsung-hdptx; modprobe rockchipdrm; sleep 2' if a.kms or a.planes else ''
lines=['wait-serial root@','serial mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t tmpfs none /tmp; modprobe fixed; modprobe panthor'+display+'; stty -echo\\n','serial : > /tmp/demo.b64\\n']
for i in range(0,len(payload),500):lines.append('serial printf %s '+payload[i:i+500]+' >> /tmp/demo.b64\\n')
command = 'serial python3 -c \'import base64,zlib;exec(zlib.decompress(base64.b64decode(open("/tmp/demo.b64").read())))\''
if a.kms or a.planes:
    # the picture is taken from the emulated HDMI monitor while the guest holds it on screen
    lines += [command.replace('serial ','serial KMS_HOLD=3 '+('KMS_SCENE=bottom-alpha ' if a.bottom_alpha else ''),1) + '; echo PANTHOR-DONE-$((6*7))\\n', 'wait-serial ' + marker, 'sleep 1', 'screenshot ' + str(a.output.resolve()), 'wait-serial PANTHOR-DONE-42', 'quit']
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
    if a.kms or a.planes:
        if result.returncode or marker not in log or not a.output.exists():
            raise SystemExit('KMS demo failed; see '+str(logpath))
        print(('KMS overlay planes' if a.planes else 'Mesa GLES rendered')+' on the HDMI screen (rockchipdrm/VOP2); screenshot: '+str(a.output))
        if a.planes:check_planes(a.output,a.bottom_alpha)
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
