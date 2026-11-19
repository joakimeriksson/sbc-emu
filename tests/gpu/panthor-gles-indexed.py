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
context = function(egl, 'eglCreateContext', P, [P, P, P, C.POINTER(I)])(display, config, None, (I * 3)(0x3098, 3, 0x3038))
assert context, hex(error())
assert function(egl, 'eglMakeCurrent', U, [P, P, P, P])(display, None, None, context), hex(error())
string = function(gl, 'glGetString', C.c_char_p, [U])
renderer = string(0x1f01).decode()
print('GL_RENDERER=' + renderer, flush=True)
assert 'Mali' in renderer or 'Panfrost' in renderer, 'software fallback: ' + renderer
texture = U()
function(gl, 'glGenTextures', None, [I, C.POINTER(U)])(1, C.byref(texture))
function(gl, 'glBindTexture', None, [U, U])(0xde1, texture)
function(gl, 'glTexImage2D', None, [U, I, I, I, I, I, U, U, P])(0xde1, 0, 0x1908, 16, 16, 0, 0x1908, 0x1401, None)
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
program = function(gl, 'glCreateProgram', U, [])()
for ident in [shader(0x8b31,b'attribute vec2 position; void main(){gl_Position=vec4(position,0.0,1.0);}'),shader(0x8b30,b'precision mediump float; void main(){gl_FragColor=vec4(0.0,1.0,0.0,1.0);}')]:
    function(gl,'glAttachShader',None,[U,U])(program,ident)
function(gl,'glBindAttribLocation',None,[U,U,C.c_char_p])(program,0,b'position')
function(gl,'glLinkProgram',None,[U])(program)
ok=I()
function(gl,'glGetProgramiv',None,[U,U,C.POINTER(I)])(program,0x8b82,C.byref(ok))
assert ok.value,'program link failed'
function(gl,'glUseProgram',None,[U])(program)

# Real buffer objects exercise element-buffer byte offsets and sparse vertex IDs.
buffers=(U*3)()
function(gl,'glGenBuffers',None,[I,C.POINTER(U)])(3,buffers)
bind=function(gl,'glBindBuffer',None,[U,U])
upload=function(gl,'glBufferData',None,[U,C.c_ssize_t,P,U])
attrib=function(gl,'glVertexAttribPointer',None,[U,I,U,U,I,P])
enable=function(gl,'glEnableVertexAttribArray',None,[U])
arrays=function(gl,'glDrawArrays',None,[U,I,I])
elements=function(gl,'glDrawElements',None,[U,I,U,P])
function(gl,'glViewport',None,[I,I,I,I])(0,0,16,16)
background=(32,128,223,255)

def positions(scale):
    data=(F*8)(99,99,-scale,-scale,scale,-scale,0,scale)
    bind(0x8892,buffers[0])
    upload(0x8892,C.sizeof(data),data,0x88e4)
    attrib(0,2,0x1406,0,0,None)
    enable(0)

def start():
    function(gl,'glClearColor',None,[F,F,F,F])(*(c/255 for c in background))
    function(gl,'glClear',None,[U])(0x4000)

def check(label, scale=0.75, shifts=(0,)):
    function(gl,'glFinish',None,[])()
    pixels=(C.c_ubyte*1024)()
    function(gl,'glReadPixels',None,[I,I,I,I,U,U,P])(0,0,16,16,0x1908,0x1401,pixels)
    assert function(gl,'glGetError',U,[])()==0, label
    a,b=8*(1-scale),8*(1+scale)
    for y in range(16):
        for x in range(16):
            px,py=x+0.5,y+0.5
            inside=any(a<=py<=b and a+(py-a)/2+8*shift<=px<=b-(py-a)/2+8*shift for shift in shifts)
            expected=(0,255,0,255) if inside else background
            actual=list(pixels[(y*16+x)*4:(y*16+x+1)*4])
            assert all(abs(actual[n]-expected[n])<=1 for n in range(4)),(label,x,y,actual,expected)
    print(label,flush=True)

positions(0.75)
start()
arrays(4,1,3)
check('ARRAY_FIRST_OK')
for kind,ctype in [(0x1401,C.c_ubyte),(0x1403,C.c_ushort),(0x1405,U)]:
    indices=(ctype*5)(99,99,3,1,2)
    bind(0x8893,buffers[1])
    upload(0x8893,C.sizeof(indices),indices,0x88e4)
    start()
    elements(4,3,kind,P(2*C.sizeof(ctype)))
    check('ELEMENTS_%d_OK'%C.sizeof(ctype))
# An array draw after indexed draws must ignore the stale index-buffer address.
start()
arrays(4,1,3)
check('ARRAY_AFTER_ELEMENTS_OK')
# Mesa exposes the GLES base-vertex extension on the G610. Resolve its actual
# extension entry point, then exercise positive and negative signed additions.
getproc=function(egl,'eglGetProcAddress',P,[C.c_char_p])
address=getproc(b'glDrawElementsBaseVertexOES')
assert address,'missing OES_draw_elements_base_vertex'
base_elements=C.CFUNCTYPE(None,U,I,U,P,I)(address)
for base,values in [(1,(2,0,1)),(-1,(4,2,3))]:
    indices=(U*5)(99,99,*values)
    bind(0x8893,buffers[1])
    upload(0x8893,C.sizeof(indices),indices,0x88e4)
    start()
    base_elements(4,3,0x1405,P(8),base)
    check('ELEMENTS_BASE_%d_OK'%base)
# Restore the ordinary indices used by the instanced cases below.
indices=(U*5)(99,99,3,1,2)
upload(0x8893,C.sizeof(indices),indices,0x88e4)

program=function(gl,'glCreateProgram',U,[])()
for ident in [shader(0x8b31,b'attribute vec2 position; attribute vec2 offset; void main(){gl_Position=vec4(position+offset,0.0,1.0);}'),shader(0x8b30,b'precision mediump float; void main(){gl_FragColor=vec4(0.0,1.0,0.0,1.0);}')]:
    function(gl,'glAttachShader',None,[U,U])(program,ident)
for location,name in [(0,b'position'),(1,b'offset')]:
    function(gl,'glBindAttribLocation',None,[U,U,C.c_char_p])(program,location,name)
function(gl,'glLinkProgram',None,[U])(program)
function(gl,'glGetProgramiv',None,[U,U,C.POINTER(I)])(program,0x8b82,C.byref(ok))
assert ok.value,'instanced link failed'
function(gl,'glUseProgram',None,[U])(program)
positions(0.25)
offsets=(F*4)(-0.5,0,0.5,0)
bind(0x8892,buffers[2])
upload(0x8892,C.sizeof(offsets),offsets,0x88e4)
attrib(1,2,0x1406,0,0,None)
enable(1)
for divisor,instances in [(1,2),(2,4),(4,6)]:
    function(gl,'glVertexAttribDivisor',None,[U,U])(1,divisor)
    start()
    function(gl,'glDrawArraysInstanced',None,[U,I,I,I])(4,1,3,instances)
    check('ARRAY_INSTANCED_%d_OK'%divisor,0.25,(-0.5,0.5))
    start()
    function(gl,'glDrawElementsInstanced',None,[U,I,U,P,I])(4,3,0x1405,P(8),instances)
    check('ELEMENTS_INSTANCED_%d_OK'%divisor,0.25,(-0.5,0.5))
print('PANTHOR_GLES_INDEXED_OK',flush=True)
