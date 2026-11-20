#!/usr/bin/env python3
"""Mesa GLES on the HDMI screen: render with the Mali-G610 (Panthor) into a scanout buffer
of the Rockchip display (rockchipdrm, VOP2) and show it with KMS. Mesa pairs the two drivers
through its kmsro path, as on a real board. Software-renderer fallback is rejected."""
import ctypes as C, os, time
os.environ.setdefault('PAN_MESA_DEBUG', 'sync,linear')
egl = C.CDLL('libEGL.so.1')
gl = C.CDLL('libGLESv2.so.2')
gbm = C.CDLL('libgbm.so.1')
drm = C.CDLL('libdrm.so.2')

def function(lib, name, result, args):
    f = getattr(lib, name)
    f.restype = result
    f.argtypes = args
    return f
P = C.c_void_p
I = C.c_int
U = C.c_uint
F = C.c_float
class ModeInfo(C.Structure):
    _fields_ = [('clock', C.c_uint32)] + [(n, C.c_uint16) for n in ('hdisplay', 'hsync_start', 'hsync_end', 'htotal', 'hskew', 'vdisplay', 'vsync_start', 'vsync_end', 'vtotal', 'vscan')] + [('vrefresh', C.c_uint32), ('flags', C.c_uint32), ('type', C.c_uint32), ('name', C.c_char * 32)]
class Resources(C.Structure):
    _fields_ = [('count_fbs', I), ('fbs', P), ('count_crtcs', I), ('crtcs', C.POINTER(C.c_uint32)), ('count_connectors', I), ('connectors', C.POINTER(C.c_uint32)), ('count_encoders', I), ('encoders', P), ('min_width', C.c_uint32), ('max_width', C.c_uint32), ('min_height', C.c_uint32), ('max_height', C.c_uint32)]
class Connector(C.Structure):
    _fields_ = [('connector_id', C.c_uint32), ('encoder_id', C.c_uint32), ('connector_type', C.c_uint32), ('connector_type_id', C.c_uint32), ('connection', I), ('mmWidth', C.c_uint32), ('mmHeight', C.c_uint32), ('subpixel', I), ('count_modes', I), ('modes', C.POINTER(ModeInfo))]
get_resources = function(drm, 'drmModeGetResources', C.POINTER(Resources), [I])
# the display device (rockchipdrm) is the card with KMS resources; Panthor's is render-only
for card in sorted(n for n in os.listdir('/dev/dri') if n.startswith('card')):
    fd = os.open('/dev/dri/' + card, os.O_RDWR)
    r = get_resources(fd)
    if r:
        break
    os.close(fd)
else:
    raise SystemExit('no KMS device')
res = r.contents
print('KMS device /dev/dri/' + card, flush=True)
conn = function(drm, 'drmModeGetConnector', C.POINTER(Connector), [I, C.c_uint32])(fd, res.connectors[0]).contents
assert conn.connection == 1 and conn.count_modes, 'HDMI not connected'
mode = conn.modes[0]
W, H = mode.hdisplay, mode.vdisplay
print('KMS mode %dx%d@%d on connector %d, CRTC %d' % (W, H, mode.vrefresh, conn.connector_id, res.crtcs[0]), flush=True)
dev = function(gbm, 'gbm_create_device', P, [I])(fd)
assert dev, 'gbm_create_device failed'
XRGB8888 = 0x34325258
surface = function(gbm, 'gbm_surface_create', P, [P, U, U, U, U])(dev, W, H, XRGB8888, 1 | 4)  # SCANOUT | RENDERING
assert surface, 'gbm_surface_create failed'
display = function(egl, 'eglGetPlatformDisplay', P, [U, P, P])(0x31d7, dev, None)
major = I()
minor = I()
error = function(egl, 'eglGetError', U, [])
assert function(egl, 'eglInitialize', U, [P, C.POINTER(I), C.POINTER(I)])(display, C.byref(major), C.byref(minor)), hex(error())
assert function(egl, 'eglBindAPI', U, [U])(0x30a0), hex(error())
attrs = (I * 15)(0x3033, 4, 0x3040, 4, 0x3024, 8, 0x3023, 8, 0x3022, 8, 0x3021, 0, 0x3025, 0, 0x3038)
configs = (P * 64)()
count = I()
assert function(egl, 'eglChooseConfig', U, [P, C.POINTER(I), C.POINTER(P), I, C.POINTER(I)])(display, attrs, configs, 64, C.byref(count)) and count.value, hex(error())
visual = I()
get_attrib = function(egl, 'eglGetConfigAttrib', U, [P, P, I, C.POINTER(I)])
config = next(c for c in configs[:count.value] if get_attrib(display, c, 0x302e, C.byref(visual)) and visual.value == XRGB8888)
window = function(egl, 'eglCreateWindowSurface', P, [P, P, P, P])(display, config, surface, None)
assert window, hex(error())
context = function(egl, 'eglCreateContext', P, [P, P, P, C.POINTER(I)])(display, config, None, (I * 3)(0x3098, 2, 0x3038))
assert context, hex(error())
assert function(egl, 'eglMakeCurrent', U, [P, P, P, P])(display, window, window, context), hex(error())
string = function(gl, 'glGetString', C.c_char_p, [U])
renderer = string(0x1f01).decode()
print('GL_RENDERER=' + renderer, flush=True)
assert 'Mali' in renderer or 'Panfrost' in renderer, 'software fallback: ' + renderer
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
function(gl,'glViewport',None,[I,I,I,I])(0,0,W,H)
def draw(points,color):
    if color not in programs:
        fragment_shader=shader(0x8b30,('precision mediump float;void main(){gl_FragColor=vec4(%s);}'%','.join(str(v) for v in (color+(1.0,) if len(color)==3 else color))).encode())
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
# A real sampled texture on the foreground floor exercises the secondary vertex
# stage, varying interpolation and native texture instructions in the same batch
# as the flat-color objects. The CPU only uploads the source checker texture.
floor_program=function(gl,'glCreateProgram',U,[])()
for ident in [shader(0x8b31,b'attribute vec2 position; varying highp vec2 uv;void main(){gl_Position=vec4(position,0.0,1.0);uv=position*0.5+vec2(0.5);}'),shader(0x8b30,b'precision mediump float; varying highp vec2 uv;uniform sampler2D tex;void main(){gl_FragColor=texture2D(tex,uv);}')]:
    function(gl,'glAttachShader',None,[U,U])(floor_program,ident)
function(gl,'glBindAttribLocation',None,[U,U,C.c_char_p])(floor_program,0,b'position')
function(gl,'glLinkProgram',None,[U])(floor_program)
ok=I();function(gl,'glGetProgramiv',None,[U,U,C.POINTER(I)])(floor_program,0x8b82,C.byref(ok))
assert ok.value,'floor program link failed'
checker=U();function(gl,'glGenTextures',None,[I,C.POINTER(U)])(1,C.byref(checker))
function(gl,'glBindTexture',None,[U,U])(0xde1,checker)
for key,value in [(0x2800,0x2600),(0x2801,0x2600),(0x2802,0x812f),(0x2803,0x812f)]:
    function(gl,'glTexParameteri',None,[U,U,I])(0xde1,key,value)
palette=[(10,15,36,255),(19,40,66,255)]
texels=(C.c_ubyte*(32*32*4))(*(c for y in range(32) for x in range(32) for c in palette[(x+y)%2]))
function(gl,'glTexImage2D',None,[U,I,I,I,I,I,U,U,P])(0xde1,0,0x1908,32,32,0,0x1908,0x1401,texels)
def floor(points):
    function(gl,'glUseProgram',None,[U])(floor_program)
    positions[:]=[v for xy in points for v in xy]
    function(gl,'glDrawArrays',None,[U,I,I])(4,0,3)
background=(0.025,0.035,0.08)
function(gl,'glClearColor',None,[F,F,F,F])(*background,1)
function(gl,'glClear',None,[U])(0x4000)
# Sun, mountain faces and a geometric prism; all are native GLES triangle draws.
sun=(0.96,0.48,0.18)
# Three translucent disks blend into a halo before the opaque scene is drawn.
function(gl,'glEnable',None,[U])(0xbe2)
function(gl,'glBlendFuncSeparate',None,[U,U,U,U])(0x302,0x303,1,0x303)
for radius,alpha in [(0.46,0.025),(0.40,0.05),(0.34,0.15)]:
    for n in range(16):
        a=n*math.tau/16;b=(n+1)*math.tau/16
        draw([(0.48,0.43),(0.48+radius*math.cos(a),0.43+radius*math.sin(a)),(0.48+radius*math.cos(b),0.43+radius*math.sin(b))],sun+(alpha,))
function(gl,'glDisable',None,[U])(0xbe2)
for n in range(16):
    a=n*math.tau/16;b=(n+1)*math.tau/16
    draw([(0.48,0.43),(0.48+0.28*math.cos(a),0.43+0.28*math.sin(a)),(0.48+0.28*math.cos(b),0.43+0.28*math.sin(b))],sun)
draw([(-1.,-0.8),(-0.5,0.25),(0.15,-0.8)],(0.05,0.18,0.31))
draw([(-0.5,0.25),(0.15,-0.8),(0.45,-0.8)],(0.035,0.10,0.22))
draw([(0.0,-0.8),(0.62,0.10),(1.0,-0.8)],(0.12,0.08,0.25))
floor([(-1.,-1.),(1.,-1.),(1.,-0.8)])
floor([(-1.,-1.),(1.,-0.8),(-1.,-0.8)])
draw([(0.,0.72),(-0.52,-0.05),(0.,-0.78)],(0.10,0.82,0.96))
draw([(0.,0.72),(0.,-0.78),(0.52,-0.05)],(0.48,0.23,0.86))
draw([(0.,0.72),(-0.52,-0.05),(-0.18,0.03)],(0.64,0.96,1.0))
function(gl,'glFinish',None,[])()
pixels=(C.c_ubyte*4)()
read=function(gl,'glReadPixels',None,[I,I,I,I,U,U,P])
# interior probes (as fractions of the screen), read back from the scanout buffer itself
for fx,fy,color in [(18/512,482/512,background),(382/512,378/512,sun),(214/512,250/512,(0.10,0.82,0.96)),(302/512,250/512,(0.48,0.23,0.86))]:
    read(int(fx*W),int(fy*H),1,1,0x1908,0x1401,pixels)
    expected=[round(c*255) for c in color]
    assert all(abs(a-b)<=1 for a,b in zip(list(pixels)[:3],expected)),(fx,fy,list(pixels),expected)
assert function(gl,'glGetError',U,[])()==0
assert function(egl,'eglSwapBuffers',U,[P,P])(display,window),hex(error())
bo=function(gbm,'gbm_surface_lock_front_buffer',P,[P])(surface)
assert bo,'no front buffer'
handle=function(gbm,'gbm_bo_get_handle',C.c_uint64,[P])(bo)&0xffffffff
stride=function(gbm,'gbm_bo_get_stride',U,[P])(bo)
fb=C.c_uint32()
assert function(drm,'drmModeAddFB',I,[I,C.c_uint32,C.c_uint32,C.c_uint8,C.c_uint8,C.c_uint32,C.c_uint32,C.POINTER(C.c_uint32)])(fd,W,H,24,32,stride,handle,C.byref(fb))==0,'drmModeAddFB failed'
connector=C.c_uint32(conn.connector_id)
rc=function(drm,'drmModeSetCrtc',I,[I,C.c_uint32,C.c_uint32,C.c_uint32,C.c_uint32,C.POINTER(C.c_uint32),I,C.POINTER(ModeInfo)])(fd,res.crtcs[0],fb.value,0,0,C.byref(connector),1,C.byref(mode))
assert rc==0,'drmModeSetCrtc failed: %d'%rc
print('PANTHOR_KMS_OK %dx%d fb %d stride %d' % (W,H,fb.value,stride),flush=True)
time.sleep(float(os.environ.get('KMS_HOLD','3')))
print('KMS_END',flush=True)
