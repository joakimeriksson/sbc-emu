#!/usr/bin/env python3
"""Submit a linear RGBA8 clear pass through Linux Panthor (no Mesa/shaders)."""
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
# Mesa-assembled Valhall shader; source is valhall/triangle.asm.
shader=[41024977855578240, 41026077367206017, 41033773948600450, 41034873460228227, 41035972971856004, 41037072483483781, 41038171995111558, 41039271506739335, 41040371018367112, 50318050134264329, 50316950622768136, 50316950622113284, 46375476314296323, 46375476314243588, 68897614812200962, 68897614800011267, 68897614800011268, 8936830510564102, 41030475413717200, 27375675807629312, 8647122390783885312, 41027176878833802, 41028276390461579, 41029375902089356, 41030475413717200, 27375675807629312, 8647122390783885312]
binary=struct.pack('<%dQ'%len(shader),*shader)
buf[0x80000:0x80000+len(binary)]=binary
buf[0x90000:0x90020]=struct.pack('<8I',0x18,0,va+0x80000,0,0,0,0,0)
w=h=128
fb=0x400000
uniform_base=0x100000
def ins(op,dst=0,src=0,imm=0):return op<<56|dst<<48|src<<40|imm
cmd=[ins(1,16,0,va+0x90000),ins(2,9,0,8<<24)]
for r in [33,34,35,36]:cmd.append(ins(2,r,0,0))
for r in [37,38,39]:cmd.append(ins(2,r,0,1))
for y in range(h):
    for x in range(w):
        n=y*w+x;addr=va+fb+n*16
        u=struct.pack('<Q14f',addr,(x+0.5)*2/w-1,1-(y+0.5)*2/h,1/1.4,0.6/1.4,1/1.5,0.7/1.5,-0.7/1.5,0,0.04,0.08,0.18,0,0,0)
        offset=uniform_base+n*64;buf[offset:offset+64]=u
        cmd.extend([ins(2,8,0,va+offset),ins(4)])
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
