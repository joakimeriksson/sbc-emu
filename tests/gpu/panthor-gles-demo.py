#!/usr/bin/env python3
"""Exercise real Mesa EGL/GLES on Panthor. Software-renderer fallback is rejected."""
import ctypes as C, os
os.environ.setdefault('EGL_LOG_LEVEL', 'debug')
os.environ.setdefault('PAN_MESA_DEBUG', 'sync,linear')
egl = C.CDLL('libEGL.so.1')
gl = C.CDLL('libGLESv2.so.2')
gbm = C.CDLL('libgbm.so.1')

def function(lib, name, result, args):
    f = getattr(lib, name)
    f.restype = result
    f.argtypes = args
    return f
P = C.c_void_p
I = C.c_int
U = C.c_uint
F = C.c_float
fd = os.open('/dev/dri/renderD128', os.O_RDWR)
dev = function(gbm, 'gbm_create_device', P, [I])(fd)
assert dev, 'gbm_create_device failed'
display = function(egl, 'eglGetPlatformDisplay', P, [U, P, P])(0x31d7, dev, None)
major = I()
minor = I()
error = function(egl, 'eglGetError', U, [])
assert function(egl, 'eglInitialize', U, [P, C.POINTER(I), C.POINTER(I)])(display, C.byref(major), C.byref(minor)), hex(error())
assert function(egl, 'eglBindAPI', U, [U])(0x30a0), hex(error())
attrs = (I * 15)(0x3033, 0, 0x3040, 4, 0x3024, 8, 0x3023, 8, 0x3022, 8, 0x3021, 8, 0x3025, 0, 0x3038)
config = P()
count = I()
assert function(egl, 'eglChooseConfig', U, [P, C.POINTER(I), C.POINTER(P), I, C.POINTER(I)])(display, attrs, C.byref(config), 1, C.byref(count)) and count.value, hex(error())
context = function(egl, 'eglCreateContext', P, [P, P, P, C.POINTER(I)])(display, config, None, (I * 3)(0x3098, 2, 0x3038))
assert context, hex(error())
assert function(egl, 'eglMakeCurrent', U, [P, P, P, P])(display, None, None, context), hex(error())
string = function(gl, 'glGetString', C.c_char_p, [U])
renderer = string(0x1f01).decode()
print('GL_RENDERER=' + renderer, flush=True)
assert 'Mali' in renderer or 'Panfrost' in renderer, 'software fallback: ' + renderer
texture = U()
function(gl, 'glGenTextures', None, [I, C.POINTER(U)])(1, C.byref(texture))
function(gl, 'glBindTexture', None, [U, U])(0xde1, texture)
function(gl, 'glTexImage2D', None, [U, I, I, I, I, I, U, U, P])(0xde1, 0, 0x1908, 512, 512, 0, 0x1908, 0x1401, None)
fbo = U()
function(gl, 'glGenFramebuffers', None, [I, C.POINTER(U)])(1, C.byref(fbo))
function(gl, 'glBindFramebuffer', None, [U, U])(0x8d40, fbo)
function(gl, 'glFramebufferTexture2D', None, [U, U, U, U, I])(0x8d40, 0x8ce0, 0xde1, texture, 0)
assert function(gl, 'glCheckFramebufferStatus', U, [U])(0x8d40) == 0x8cd5

def shader(kind, source):
    ident = function(gl, 'glCreateShader', U, [U])(kind)
    text = C.c_char_p(source)
    function(gl, 'glShaderSource', None, [U, I, C.POINTER(C.c_char_p), P])(ident, 1, C.byref(text), None)
    function(gl, 'glCompileShader', None, [U])(ident)
    ok = I()
    function(gl, 'glGetShaderiv', None, [U,U,C.POINTER(I)])(ident,0x8b81,C.byref(ok))
    assert ok.value, 'shader compile failed'
    return ident

import math,struct,zlib,base64,binascii
vertex_shader=shader(0x8b31,b'attribute vec2 position; void main(){gl_Position=vec4(position,0.0,1.0);}')
programs={}
positions=(F*6)()
function(gl,'glVertexAttribPointer',None,[U,I,U,U,I,P])(0,2,0x1406,0,0,positions)
function(gl,'glEnableVertexAttribArray',None,[U])(0)
function(gl,'glViewport',None,[I,I,I,I])(0,0,512,512)
def draw(points,color):
    if color not in programs:
        fragment_shader=shader(0x8b30,('precision mediump float;void main(){gl_FragColor=vec4(%s,1.0);}'%','.join(str(v) for v in color)).encode())
        program=function(gl,'glCreateProgram',U,[])()
        for ident in [vertex_shader,fragment_shader]:function(gl,'glAttachShader',None,[U,U])(program,ident)
        function(gl,'glBindAttribLocation',None,[U,U,C.c_char_p])(program,0,b'position')
        function(gl,'glLinkProgram',None,[U])(program)
        ok=I();function(gl,'glGetProgramiv',None,[U,U,C.POINTER(I)])(program,0x8b82,C.byref(ok))
        assert ok.value,'program link failed'
        programs[color]=program
    function(gl,'glUseProgram',None,[U])(programs[color])
    positions[:]=[v for xy in points for v in xy]
    function(gl,'glDrawArrays',None,[U,I,I])(4,0,3)
background=(0.025,0.035,0.08)
function(gl,'glClearColor',None,[F,F,F,F])(*background,1)
function(gl,'glClear',None,[U])(0x4000)
# Sun, mountain faces and a geometric prism; all are native GLES triangle draws.
sun=(0.96,0.48,0.18)
for n in range(16):
    a=n*math.tau/16;b=(n+1)*math.tau/16
    draw([(0.48,0.43),(0.48+0.28*math.cos(a),0.43+0.28*math.sin(a)),(0.48+0.28*math.cos(b),0.43+0.28*math.sin(b))],sun)
draw([(-1.,-0.8),(-0.5,0.25),(0.15,-0.8)],(0.05,0.18,0.31))
draw([(-0.5,0.25),(0.15,-0.8),(0.45,-0.8)],(0.035,0.10,0.22))
draw([(0.0,-0.8),(0.62,0.10),(1.0,-0.8)],(0.12,0.08,0.25))
draw([(-1.,-1.),(1.,-1.),(1.,-0.8)],(0.04,0.06,0.14))
draw([(-1.,-1.),(1.,-0.8),(-1.,-0.8)],(0.04,0.06,0.14))
draw([(0.,0.72),(-0.52,-0.05),(0.,-0.78)],(0.10,0.82,0.96))
draw([(0.,0.72),(0.,-0.78),(0.52,-0.05)],(0.48,0.23,0.86))
draw([(0.,0.72),(-0.52,-0.05),(-0.18,0.03)],(0.64,0.96,1.0))
function(gl,'glFinish',None,[])()
pixels=(C.c_ubyte*(512*512*4))()
function(gl,'glReadPixels',None,[I,I,I,I,U,U,P])(0,0,512,512,0x1908,0x1401,pixels)
assert function(gl,'glGetError',U,[])()==0
# Interior probes span different programs, overlaps and untouched background.
for x,y,color in [(18,482,background),(382,378,sun),(214,250,(0.10,0.82,0.96)),(302,250,(0.48,0.23,0.86)),(18,18,(0.04,0.06,0.14))]:
    actual=list(pixels[(y*512+x)*4:(y*512+x+1)*4])
    expected=[round(c*255) for c in color]+[255]
    assert all(abs(a-b)<=1 for a,b in zip(actual,expected)),(x,y,actual,expected)
def chunk(kind,data):return struct.pack('>I',len(data))+kind+data+struct.pack('>I',binascii.crc32(kind+data)&0xffffffff)
raw=b''.join(b'\0'+bytes(pixels[y*512*4:(y+1)*512*4]) for y in range(511,-1,-1))
png=b'\x89PNG\r\n\x1a\n'+chunk(b'IHDR',struct.pack('>IIBBBBB',512,512,8,6,0,0,0))+chunk(b'IDAT',zlib.compress(raw))+chunk(b'IEND',b'')
print('PANTHOR_GLES_DEMO_OK',flush=True)
print('PNG_BASE64='+base64.b64encode(png).decode(),flush=True)
print('IMAGE_END',flush=True)
