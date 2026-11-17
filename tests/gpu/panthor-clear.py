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
grp=pan(7,array(q,8)+struct.pack('<BBBBIQQQII',0,1,0,1,0,0,1,0,vm,0))
gid=struct.unpack_from('<I',grp,52)[0]
w,h=320,180
fb=0x400000
cmd=[]
def ins(op,dst=0,src=0,imm=0):return op<<56|dst<<48|src<<40|imm
# A retro landscape assembled from 2x2 clear regions. CPU supplies colors;
# the emulated fragment engine writes all pixels via RUN_FRAGMENT descriptors.
import math
cmd=[]
expected=bytearray(w*h*4)
for y in range(0,h,2):
    for x in range(0,w,2):
        t=y/h
        color=[int(25+65*t),int(16+35*t),int(65+50*t)]
        if (x-236)**2+(y-51)**2<27**2:
            color=[255,int(200-100*y/h),90]
            if y>50 and y%8<2: color=[62,30,88]
        horizon=101+int(10*math.sin(x/21)+15*math.sin(x/46+1))
        if y>horizon: color=[32,25,66]
        if y>121:
            color=[16,31+int(20*(y-121)/59),65+int(20*(y-121)/59)]
            if y%10<2:color=[51,49,102]
            if abs(x-236)<(y-110)*0.45 and y%6<2:color=[206,87,115]
        # An illuminated geometric tower, with a shaded side and lit windows.
        if 56<=x<104 and 45<=y<151:
            color=[34,40,83] if x<88 else [19,25,54]
            if x<88 and x%10<4 and y%12<6:color=[54,205,199]
        if 48<=x<112 and 145<=y<153:color=[51,73,113]
        rgba=color[0]|color[1]<<8|color[2]<<16|255<<24
        for row_y in (y,y+1):
            expected[(row_y*w+x)*4:(row_y*w+x+2)*4]=struct.pack("<I",rgba)*2
        f=[0]*48
        f[8]=(w-1)|((h-1)<<16); f[9]=x|(y<<16); f[10]=(x+1)|((y+1)<<16); f[12]=1<<29
        f[32]=1<<26; f[33]=1|(19<<3)|(2<<8)|(0x688<<16)
        f[40]=va+fb; f[42]=w*4; f[44:48]=[rgba]*4
        desc=0x100000+(len(cmd)//4)*192
        buf[desc:desc+192]=struct.pack('<48I',*f)
        cmd.extend([ins(1,40,0,va+desc),ins(2,42,0,0),ins(2,43,0,(w-1)|((h-1)<<16)),ins(7)])
code=struct.pack('<%dQ'%len(cmd),*cmd);buf[:len(code)]=code
# A DRM syncobj proves completion, not just an early framebuffer write.
sync=struct.unpack_from('<I',ioctl(0xbf,bytes(8)))[0]
syn=struct.pack('<IIQ',1<<31,sync,0)
submit=struct.pack('<IIQII',0,len(code),va,0,0)+array(syn,16)
pan(9,struct.pack('<II',gid,0)+array(submit,40))
handles=ptr(struct.pack('<I',sync))
ioctl(0xc3,struct.pack('<QqIIII',handles,time.monotonic_ns()+5_000_000_000,1,1,0,0))
pixels=bytes(buf[fb:fb+w*h*4])
assert pixels==expected,'framebuffer mismatch'
assert pixels[:4]==bytes([25,16,65,255]),'background mismatch'
assert pixels[(60*w+60)*4:(60*w+60)*4+4]==bytes([54,205,199,255]),'tower mismatch'
assert len(set(pixels[i:i+4] for i in range(0,len(pixels),4)))>100
import binascii
def chunk(kind,data):return struct.pack('>I',len(data))+kind+data+struct.pack('>I',binascii.crc32(kind+data)&0xffffffff)
raw=b''.join(b'\0'+pixels[y*w*4:(y+1)*w*4] for y in range(h))
png=b'\x89PNG\r\n\x1a\n'+chunk(b'IHDR',struct.pack('>IIBBBBB',w,h,8,6,0,0,0))+chunk(b'IDAT',zlib.compress(raw))+chunk(b'IEND',b'')
print('PANTHOR_CLEAR_OK',flush=True)
print('PNG_BASE64='+base64.b64encode(png).decode(),flush=True)
print('IMAGE_END',flush=True)
