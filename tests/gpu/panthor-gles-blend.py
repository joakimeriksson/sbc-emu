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
for ident in [shader(0x8b31,b'attribute vec2 position; void main(){gl_Position=vec4(position,0.0,1.0);}'),shader(0x8b30,b'precision mediump float; uniform vec4 color; void main(){gl_FragColor=color;}')]:
    function(gl,'glAttachShader',None,[U,U])(program,ident)
function(gl,'glBindAttribLocation',None,[U,U,C.c_char_p])(program,0,b'position')
function(gl,'glLinkProgram',None,[U])(program)
ok=I()
function(gl,'glGetProgramiv',None,[U,U,C.POINTER(I)])(program,0x8b82,C.byref(ok))
assert ok.value,'program link failed'
function(gl,'glUseProgram',None,[U])(program)

positions=(F*6)()
function(gl,'glVertexAttribPointer',None,[U,I,U,U,I,P])(0,2,0x1406,0,0,positions)
function(gl,'glEnableVertexAttribArray',None,[U])(0)
function(gl,'glViewport',None,[I,I,I,I])(0,0,16,16)
function(gl,'glDisable',None,[U])(0xbd0) # dither off for an exact RGBA8 oracle
function(gl,'glEnable',None,[U])(0xbe2)
location=function(gl,'glGetUniformLocation',I,[U,C.c_char_p])(program,b'color')
assert location>=0
background=(32,128,223,192)
constant=(0.25,)*4
function(gl,'glBlendColor',None,[F,F,F,F])(*constant)
# API factors/equations form an independent oracle, not the GPU A+B*C encoding.
ZERO,ONE,SA,ISA,DA,IDC,SC,ISC,DC,CC,ICC=0,1,0x302,0x303,0x304,0x307,0x300,0x301,0x306,0x8001,0x8002
ADD,SUB,REV=0x8006,0x800a,0x800b
cases=[('straight',SA,ISA,SA,ISA,ADD,15),('premult',ONE,ISA,ONE,ISA,ADD,15),('add',ONE,ONE,ONE,ONE,ADD,15),('multiply',DC,ZERO,DC,ZERO,ADD,15),('inverse_source',ZERO,ISC,ZERO,ISC,ADD,15),('separate_alpha',SA,ISA,ONE,ISA,ADD,15),('constant',CC,ICC,CC,ICC,ADD,15),('subtract',ONE,ONE,ONE,ONE,SUB,15),('reverse',ONE,ONE,ONE,ONE,REV,15),('mask_rb',SA,ISA,ONE,ISA,ADD,5),('zero',ZERO,ZERO,ZERO,ZERO,ADD,15),('inverse_dest',IDC,ZERO,IDC,ZERO,ADD,15),('dest_alpha',DA,ZERO,DA,ZERO,ADD,15),('mask_none',SA,ISA,ONE,ISA,ADD,0)]
def factor(f,src,dst,n):
    return {ZERO:0,ONE:1,SA:src[3],ISA:1-src[3],DA:dst[3],DC:dst[n],IDC:1-dst[n],SC:src[n],ISC:1-src[n],CC:constant[n],ICC:1-constant[n]}[f]
for name,sf,df,sa,da,equation,mask in cases:
    function(gl,'glColorMask',None,[U,U,U,U])(1,1,1,1)
    function(gl,'glClearColor',None,[F,F,F,F])(*(v/255 for v in background))
    function(gl,'glClear',None,[U])(0x4000)
    function(gl,'glBlendFuncSeparate',None,[U,U,U,U])(sf,df,sa,da)
    function(gl,'glBlendEquation',None,[U])(equation)
    function(gl,'glColorMask',None,[U,U,U,U])(*(bool(mask&(1<<n)) for n in range(4)))
    draws=[(0.75,(0.75,0.25,0.125,0.5)),(0.5,(0.125,0.5,0.75,0.25)),(0.25,(0.25,0.125,0.5,0.75))]
    for scale,color in draws:
        positions[:]=[-scale,-scale,scale,-scale,0,scale]
        function(gl,'glUniform4f',None,[I,F,F,F,F])(location,*color)
        function(gl,'glDrawArrays',None,[U,I,I])(4,0,3)
    function(gl,'glFinish',None,[])()
    pixels=(C.c_ubyte*1024)()
    function(gl,'glReadPixels',None,[I,I,I,I,U,U,P])(0,0,16,16,0x1908,0x1401,pixels)
    assert function(gl,'glGetError',U,[])()==0,name
    for y in range(16):
        for x in range(16):
            px,py=x+0.5,y+0.5
            expected=list(background)
            for scale,src in draws:
                a,b=8*(1-scale),8*(1+scale)
                if not(a<=py<=b and a+(py-a)/2<=px<=b-(py-a)/2):continue
                dst=[v/255 for v in expected]
                for n in range(4):
                    if not mask&(1<<n):continue
                    left=src[n]*factor(sa if n==3 else sf,src,dst,n)
                    right=dst[n]*factor(da if n==3 else df,src,dst,n)
                    value=left+right if equation==ADD else left-right if equation==SUB else right-left
                    expected[n]=int(max(0,min(1,value))*255+0.5)
            actual=list(pixels[(y*16+x)*4:(y*16+x+1)*4])
            assert all(abs(actual[n]-expected[n])<=1 for n in range(4)),(name,x,y,actual,expected)
    print('BLEND_%s_OK'%name,flush=True)
# Two triangles share the diagonal of a translucent rectangle. Every covered
# pixel, including those exactly on the diagonal, must blend exactly once.
quad=(F*12)(-0.75,-0.75,0.75,-0.75,0.75,0.75,-0.75,-0.75,0.75,0.75,-0.75,0.75)
function(gl,'glVertexAttribPointer',None,[U,I,U,U,I,P])(0,2,0x1406,0,0,quad)
source=(0.75,0.25,0.125,0.5)
for scissor in [False,True]:
    function(gl,'glDisable',None,[U])(0xc11)
    function(gl,'glColorMask',None,[U,U,U,U])(1,1,1,1)
    function(gl,'glClearColor',None,[F,F,F,F])(*(v/255 for v in background))
    function(gl,'glClear',None,[U])(0x4000)
    if scissor:
        function(gl,'glEnable',None,[U])(0xc11)
        function(gl,'glScissor',None,[I,I,I,I])(4,4,8,8)
    function(gl,'glBlendEquation',None,[U])(ADD)
    function(gl,'glBlendFunc',None,[U,U])(SA,ISA)
    function(gl,'glUniform4f',None,[I,F,F,F,F])(location,*source)
    function(gl,'glDrawArrays',None,[U,I,I])(4,0,6)
    function(gl,'glFinish',None,[])()
    pixels=(C.c_ubyte*1024)()
    function(gl,'glReadPixels',None,[I,I,I,I,U,U,P])(0,0,16,16,0x1908,0x1401,pixels)
    assert function(gl,'glGetError',U,[])()==0
    color=[int((source[n]*0.5+(background[n]/255)*0.5)*255+0.5) for n in range(4)]
    for y in range(16):
        for x in range(16):
            inside=(4<=x<12 and 4<=y<12) if scissor else (2<=x<14 and 2<=y<14)
            expected=color if inside else background
            actual=list(pixels[(y*16+x)*4:(y*16+x+1)*4])
            assert all(abs(actual[n]-expected[n])<=1 for n in range(4)),('quad',scissor,x,y,actual,expected)
    print('BLEND_QUAD_%s_OK'%scissor,flush=True)
print('PANTHOR_GLES_BLEND_OK',flush=True)
