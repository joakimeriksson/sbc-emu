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
for ident in [shader(0x8b31,b'attribute vec2 position; void main(){gl_Position=vec4(position,0.0,1.0);}'),shader(0x8b30,b'precision mediump float; uniform sampler2D tex; uniform vec2 uv; void main(){gl_FragColor=texture2D(tex,uv);}')]:
    function(gl,'glAttachShader',None,[U,U])(program,ident)
function(gl,'glBindAttribLocation',None,[U,U,C.c_char_p])(program,0,b'position')
function(gl,'glLinkProgram',None,[U])(program)
ok=I()
function(gl,'glGetProgramiv',None,[U,U,C.POINTER(I)])(program,0x8b82,C.byref(ok))
assert ok.value,'program link failed'
function(gl,'glUseProgram',None,[U])(program)

positions=(F*6)(-0.75,-0.75,0.75,-0.75,0,0.75)
function(gl,'glVertexAttribPointer',None,[U,I,U,U,I,P])(0,2,0x1406,0,0,positions)
function(gl,'glEnableVertexAttribArray',None,[U])(0)
function(gl,'glViewport',None,[I,I,I,I])(0,0,16,16)
getuniform=function(gl,'glGetUniformLocation',I,[U,C.c_char_p])
uv_location=getuniform(program,b'uv')
tex_location=getuniform(program,b'tex')
assert uv_location>=0 and tex_location>=0
samples=(U*2)()
function(gl,'glGenTextures',None,[I,C.POINTER(U)])(2,samples)
parameter=function(gl,'glTexParameteri',None,[U,U,I])
background=(32,128,223,255)

def check(label,expected):
    function(gl,'glFinish',None,[])()
    pixels=(C.c_ubyte*1024)()
    function(gl,'glReadPixels',None,[I,I,I,I,U,U,P])(0,0,16,16,0x1908,0x1401,pixels)
    assert function(gl,'glGetError',U,[])()==0,label
    expected=tuple(int(v+0.5) for v in expected)
    for y in range(16):
        for x in range(16):
            px,py=x+0.5,y+0.5
            inside=2<=py<=14 and 2+(py-2)/2<=px<=14-(py-2)/2
            wanted=expected if inside else background
            actual=list(pixels[(y*16+x)*4:(y*16+x+1)*4])
            assert all(abs(actual[n]-wanted[n])<=1 for n in range(4)),(label,x,y,actual,wanted)
    print(label,flush=True)

frames=0
for slot,red in [(0,(255,0,0,255)),(1,(17,113,229,91))]:
    function(gl,'glActiveTexture',None,[U])(0x84c0+slot)
    function(gl,'glBindTexture',None,[U,U])(0xde1,samples[slot])
    function(gl,'glUniform1i',None,[I,I])(tex_location,slot)
    green,blue,yellow=(0,255,0,128),(0,0,255,64),(255,255,0,255)
    texels=(C.c_ubyte*16)(*(red+green+blue+yellow))
    function(gl,'glTexImage2D',None,[U,I,I,I,I,I,U,U,P])(0xde1,0,0x1908,2,2,0,0x1908,0x1401,texels)
    average=lambda *colors:tuple(sum(c[n] for c in colors)/len(colors) for n in range(4))
    for wrap in [0x812f,0x2901,0x8370]: # clamp, repeat, mirrored repeat
        for key in [0x2802,0x2803]:parameter(0xde1,key,wrap)
        for filtering in [0x2600,0x2601]:
            for key in [0x2800,0x2801]:parameter(0xde1,key,filtering)
            cases=[((0.25,0.25),red),((0.75,0.25),green),((0.25,0.75),blue),((0.75,0.75),yellow)]
            if filtering==0x2601:
                cases += [((0.5,0.5),average(red,green,blue,yellow)),((0.5,0.25),average(red,green)),((0.25,0.5),average(red,blue))]
                cases += [((0,0),average(red,green,blue,yellow) if wrap==0x2901 else red)]
            else:
                cases += [((-0.25,0.25),green if wrap==0x2901 else red),((1.25,0.25),red if wrap==0x2901 else green)]
            for uv,expected in cases:
                function(gl,'glUniform2f',None,[I,F,F])(uv_location,*uv)
                function(gl,'glClearColor',None,[F,F,F,F])(*(c/255 for c in background))
                function(gl,'glClear',None,[U])(0x4000)
                function(gl,'glDrawArrays',None,[U,I,I])(4,0,3)
                check('TEXTURE_%d_%x_%x_%s'%(slot,wrap,filtering,uv),expected)
                frames+=1
print('PANTHOR_GLES_TEXTURE_OK frames=%d'%frames,flush=True)
