#!/usr/bin/env python3
"""Submit a Mesa-assembled Valhall shader through Linux Panthor and verify its output."""
import ctypes, fcntl, mmap, os, struct, time, zlib, base64
fd=os.open('/dev/dri/renderD128',os.O_RDWR)
keep=[]
def ptr(data):
    b=ctypes.create_string_buffer(data);keep.append(b);return ctypes.addressof(b)
def array(data,stride):return struct.pack('<IIQ',stride,len(data)//stride,ptr(data))
def ioctl(n,data):
    b=bytearray(data);fcntl.ioctl(fd,0xc0006400|(len(b)<<16)|n,b,True);return b
def pan(n,data):return ioctl(0x40+n,data)
vm=struct.unpack_from('<I',pan(1,struct.pack('<IIQ',0,0,1<<32)),4)[0]
size=8<<20
bo=struct.unpack_from('<I',pan(5,struct.pack('<QIIII',size,0,vm,0,0)),16)[0]
offset=struct.unpack_from('<Q',pan(6,struct.pack('<IIQ',bo,0,0)),8)[0]
buf=mmap.mmap(fd,size,offset=offset)
va=0x100000
op=struct.pack('<IIQQQ',0,bo,0,va,size)+bytes(16)
pan(3,struct.pack('<II',vm,0)+array(op,48))
q=struct.pack('<II',0,4096)
grp=pan(7,array(q,8)+struct.pack('<BBBBIQQQII',1,0,0,1,0,1,0,0,vm,0))
gid=struct.unpack_from('<I',grp,52)[0]
# Mesa-assembled Valhall shader; source is valhall/workgroup-triangle.asm.
shader=[41024977855578240, 41026077367206017, 41041470529994813, 45263595180658447, 45263595180658447, 45263595180658447, 45263595180658447, 45263595180658447, 45263595180658447, 45263595180658447, 45263595180669967, 45263595180658447, 45263595180658447, 45263595180658447, 45263595180658447, 45247102506241792, 40752298973528124, 40753398485155901, 41042570041622658, 41043669553250435, 50322448181170184, 41042570041622660, 41043669553250437, 50323547692797961, 41035972971856006, 41037072483483783, 41038171995111560, 41039271506739337, 41040371018367114, 50318050134264329, 50316950622768136, 50316950622113284, 46375476314296323, 46375476314243588, 68897614812200962, 68897614800011267, 68897614800011268, 8936830510564102, 41030475413717200, 27375675807629312, 8647122390783885312, 41027176878833804, 41028276390461581, 41029375902089358, 41030475413717200, 27375675807629312, 8647122390783885312]
allocation=int(os.environ.get('PANTHOR_SHADER_REGISTERS','64'))
assert allocation in (32,64)
# Same shader, assembled with its two temporary registers in the upper bank.
if allocation==32:
    shader=[41024977855578240, 41026077367206017, 41041470529994813, 45263595180658447, 45263595180658447, 45263595180658447, 45263595180658447, 45263595180658447, 45263595180658447, 45263595180658447, 45263595180669967, 45263595180658447, 45263595180658447, 45263595180658447, 45263595180658447, 45247102506241792, 40752298973528124, 40753398485155901, 41077754413711490, 41078853925339267, 50322448183275528, 41077754413711492, 41078853925339269, 50323547694903305, 41035972971856006, 41037072483483783, 41038171995111560, 41039271506739337, 41040371018367114, 50318050134264329, 50316950622768136, 50316950622113284, 46375476314296323, 46375476314243588, 68897614812200962, 68897614800011267, 68897614800011268, 8936830510564102, 41030475413717200, 27375675807629312, 8647122390783885312, 41027176878833804, 41028276390461581, 41029375902089358, 41030475413717200, 27375675807629312, 8647122390783885312]
binary=struct.pack('<%dQ'%len(shader),*shader)
buf[0x80000:0x80000+len(binary)]=binary
buf[0x90000:0x90020]=struct.pack('<8I',0x18 | ((2<<30) if allocation==32 else 0),0x3000,va+0x80000,0,0,0,0,0)
w=h=128
fb=0x400000
uniform_base=0x100000
def ins(op,dst=0,src=0,imm=0):return op<<56|dst<<48|src<<40|imm
# One RUN_COMPUTE dispatch: 16x16 workgroups, each containing 8x8 invocations.
# The shader derives both coordinates and output addresses from global IDs.
u=struct.pack('<Q14f',va+fb,1/64,-1+1/128,-1/64,1-1/128,1/1.4,0.6/1.4,1/1.5,0.7/1.5,-0.7/1.5,0,0.04,0.08,0.18,0)
buf[uniform_base:uniform_base+64]=u
cmd=[ins(1,16,0,va+0x90000),ins(2,8,0,va+uniform_base),ins(2,9,0,8<<24),ins(2,33,0,7|(7<<10))]
for r,v in [(34,0),(35,0),(36,0),(37,16),(38,16),(39,1)]:cmd.append(ins(2,r,0,v))
cmd.append(ins(4))
code=struct.pack('<%dQ'%len(cmd),*cmd);buf[:len(code)]=code
# A DRM syncobj proves completion, not just an early framebuffer write.
sync=struct.unpack_from('<I',ioctl(0xbf,bytes(8)))[0]
syn=struct.pack('<IIQ',1<<31,sync,0)
submit=struct.pack('<IIQII',0,len(code),va,0,0)+array(syn,16)
pan(9,struct.pack('<II',gid,0)+array(submit,40))
handles=ptr(struct.pack('<I',sync))
ioctl(0xc3,struct.pack('<QqIIII',handles,time.monotonic_ns()+5_000_000_000,1,1,0,0))
raw=bytes(buf[fb:fb+w*h*16]);pixels=bytearray()
for y in range(h):
    for x in range(w):
        color=struct.unpack_from('<4f',raw,(y*w+x)*16)
        px=(x+0.5)*2/w-1;py=1-(y+0.5)*2/h
        b2=(py+0.6)/1.4;b1=(px+0.7-0.7*b2)/1.5;b0=1-b1-b2
        expected=(b0,b1,b2,1) if min(b0,b1,b2)>=0 else (0.04,0.08,0.18,1)
        if min(abs(b0),abs(b1),abs(b2))>1e-5:
            assert all(abs(a-b)<1e-5 for a,b in zip(color,expected)),(x,y,color,expected)
        pixels.extend(round(max(0,min(1,c))*255) for c in color)
import binascii
def chunk(kind,data):return struct.pack('>I',len(data))+kind+data+struct.pack('>I',binascii.crc32(kind+data)&0xffffffff)
raw=b''.join(b'\0'+pixels[y*w*4:(y+1)*w*4] for y in range(h))
png=b'\x89PNG\r\n\x1a\n'+chunk(b'IHDR',struct.pack('>IIBBBBB',w,h,8,6,0,0,0))+chunk(b'IDAT',zlib.compress(raw))+chunk(b'IEND',b'')
print('PANTHOR_SHADER_OK',flush=True)
print('PNG_BASE64='+base64.b64encode(png).decode(),flush=True)
print('IMAGE_END',flush=True)
