#!/usr/bin/env python3
"""Verify native position-shader instructions through Panthor compute; no rasterization."""
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
# Original position shader, adapted only to source internal ID from global X
# and bind output to ordinary structured table 3 instead of hardware table 13.
root, attributes, buffers, outputs, vertices, output = 0x20000,0x21000,0x22000,0x22500,0x23000,0x24000
for table, target in [(1,attributes),(2,buffers),(3,outputs)]:
    buf[root+table*16:root+table*16+16]=struct.pack('<QQ',(va+target)|(1<<56),32)
buf[attributes:attributes+32]=struct.pack('<8I',0x115|(((175<<12)|16)<<10),2,0,0,8,0,0,0)
buf[buffers:buffers+32]=struct.pack('<IIQ4I',0x19,24,va+vertices,0,0,0,0)
buf[outputs:outputs+32]=struct.pack('<IIQ4I',0x39,48,va+output,16,0,0,0)
buf[vertices:vertices+24]=struct.pack('<6f',-0.75,-0.75,0.75,-0.75,0.0,0.75)
buf[output-16:output+64]=b'\xa5'*80
shader=[41089849041616956, 605311954898222460, 50313652095516736, 50314751607276097, 41027176878833796, 41028276390461648, 1179525306206126971, 8674284763499987012]
binary=struct.pack('<%dQ'%len(shader),*shader)
buf[0x80000:0x80000+len(binary)]=binary
buf[0x90000:0x90020]=struct.pack('<8I',0x80000018,0x3000,va+0x80000,0,0,0,0,0)
buf[0xa0000:0xa0018]=struct.pack('<6f',8,8,8,8,0.5,0)
def ins(op,dst=0,src=0,imm=0):return op<<56|dst<<48|src<<40|imm
cmd=[ins(1,0,0,(va+root)|4),ins(1,16,0,va+0x90000),ins(2,8,0,va+0xa0000),ins(2,9,0,3<<24)]
for r,v in [(33,0),(34,0),(35,0),(36,0),(37,3),(38,1),(39,1)]:cmd.append(ins(2,r,0,v))
cmd.append(ins(4))
code=struct.pack('<%dQ'%len(cmd),*cmd);buf[:len(code)]=code
sync=struct.unpack_from('<I',ioctl(0xbf,bytes(8)))[0]
syn=struct.pack('<IIQ',1<<31,sync,0)
submit=struct.pack('<IIQII',0,len(code),va,0,0)+array(syn,16)
pan(9,struct.pack('<II',gid,0)+array(submit,40))
handles=ptr(struct.pack('<I',sync))
ioctl(0xc3,struct.pack('<QqIIII',handles,time.monotonic_ns()+5_000_000_000,1,1,0,0))
expected=[(2.0,2.0,0.5,1.0),(14.0,2.0,0.5,1.0),(8.0,14.0,0.5,1.0)]
for n,value in enumerate(expected):
    actual=struct.unpack_from('<4f',buf,output+n*16)
    assert actual==value,(n,actual,value)
assert bytes(buf[output-16:output])==b'\xa5'*16
assert bytes(buf[output+48:output+64])==b'\xa5'*16
print('POSITION_VECTORS='+str(expected),flush=True)
print('PANTHOR_POSITION_OK',flush=True)
