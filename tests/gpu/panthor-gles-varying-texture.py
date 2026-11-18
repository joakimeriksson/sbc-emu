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
for ident in [shader(0x8b31,b'attribute vec2 position; varying highp vec2 uv; void main(){gl_Position=vec4(position,0.0,1.0);uv=position*0.5+vec2(0.5);}'),shader(0x8b30,b'precision mediump float; uniform sampler2D tex; varying highp vec2 uv; void main(){gl_FragColor=texture2D(tex,uv);}')]:
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
sample=U()
function(gl,'glGenTextures',None,[I,C.POINTER(U)])(1,C.byref(sample))
function(gl,'glBindTexture',None,[U,U])(0xde1,sample)
for key,value in [(0x2800,0x2600),(0x2801,0x2600),(0x2802,0x812f),(0x2803,0x812f)]:
    function(gl,'glTexParameteri',None,[U,U,I])(0xde1,key,value)
texels=(C.c_ubyte*16)(255,0,0,255,0,255,0,255,0,0,255,255,255,255,0,255)
function(gl,'glTexImage2D',None,[U,I,I,I,I,I,U,U,P])(0xde1,0,0x1908,2,2,0,0x1908,0x1401,texels)
for filtering in [0x2600,0x2601]:
    for key in [0x2800,0x2801]:function(gl,'glTexParameteri',None,[U,U,I])(0xde1,key,filtering)
    for scale,reverse in [(0.75,False),(0.5,True),(0.75,True)]:
        vertices=[(-scale,-scale),(scale,-scale),(0.0,scale)]
        if reverse:vertices.reverse()
        positions[:]=[c for p in vertices for c in p]
        background=(32,128,223,255)
        function(gl,'glClearColor',None,[F,F,F,F])(*(c/255 for c in background))
        function(gl,'glClear',None,[U])(0x4000)
        function(gl,'glDrawArrays',None,[U,I,I])(4,0,3)
        function(gl,'glFinish',None,[])()
        pixels=(C.c_ubyte*1024)()
        function(gl,'glReadPixels',None,[I,I,I,I,U,U,P])(0,0,16,16,0x1908,0x1401,pixels)
        assert function(gl,'glGetError',U,[])()==0
        a,b=8*(1-scale),8*(1+scale)
        for y in range(16):
            for x in range(16):
                px,py=x+0.5,y+0.5
                inside=a<=py<=b and a+(py-a)/2<=px<=b-(py-a)/2
                if not inside:expected=background
                elif filtering==0x2600:expected=([(255,0,0,255),(0,255,0,255)] if y<8 else [(0,0,255,255),(255,255,0,255)])[int(x>=8)]
                else:
                    fx,fy=max(0,min(1,px/8-0.5)),max(0,min(1,py/8-0.5))
                    expected=tuple(int(c*255+0.5) for c in [(1-fx)*(1-fy)+fx*fy,fx,(1-fx)*fy,1])
                actual=list(pixels[(y*16+x)*4:(y*16+x+1)*4])
                assert all(abs(actual[n]-expected[n])<=1 for n in range(4)),(filtering,scale,reverse,x,y,actual,expected)
print('PANTHOR_GLES_VARYING_TEXTURE_OK',flush=True)
