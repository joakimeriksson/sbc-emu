#!/usr/bin/env python3
"""Overlay planes on the Rockchip display (rockchipdrm, VOP2): KMS dumb buffers, no GPU.

The primary plane is white above the middle of the screen and grey (0x80) below it. Three
overlay planes straddle the middle, left to right:
  A  XRGB8888, solid red with the X byte 0: covers what is below (red);
  B  ARGB8888 grey 0x40 at alpha 0x80, "Pre-multiplied": Cs + (1 - As) Cd = 191 / 128;
  C  the same pixels, "Coverage":                        As Cs + (1 - As) Cd = 159 / 96.
A fourth plane D (XRGB8888 red, X byte 0) sits low on the grey, on an overlay plane of the
Cluster kind when there is one (the lowest overlay zpos): Linux mixes Cluster windows by pixel
alpha whatever the format, so it shows what the hardware is told to do with the X byte."""
import ctypes as C, fcntl, mmap, os, struct, time

drm = C.CDLL('libdrm.so.2')

def function(name, result, args):
    f = getattr(drm, name)
    f.restype = result
    f.argtypes = args
    return f
P = C.c_void_p
I = C.c_int
U32 = C.c_uint32
class ModeInfo(C.Structure):
    _fields_ = [('clock', U32)] + [(n, C.c_uint16) for n in ('hdisplay', 'hsync_start', 'hsync_end', 'htotal', 'hskew', 'vdisplay', 'vsync_start', 'vsync_end', 'vtotal', 'vscan')] + [('vrefresh', U32), ('flags', U32), ('type', U32), ('name', C.c_char * 32)]
class Resources(C.Structure):
    _fields_ = [('count_fbs', I), ('fbs', P), ('count_crtcs', I), ('crtcs', C.POINTER(U32)), ('count_connectors', I), ('connectors', C.POINTER(U32)), ('count_encoders', I), ('encoders', P), ('min_width', U32), ('max_width', U32), ('min_height', U32), ('max_height', U32)]
class Connector(C.Structure):
    _fields_ = [('connector_id', U32), ('encoder_id', U32), ('connector_type', U32), ('connector_type_id', U32), ('connection', I), ('mmWidth', U32), ('mmHeight', U32), ('subpixel', I), ('count_modes', I), ('modes', C.POINTER(ModeInfo))]
class PlaneRes(C.Structure):
    _fields_ = [('count_planes', U32), ('planes', C.POINTER(U32))]
class Plane(C.Structure):
    _fields_ = [('count_formats', U32), ('formats', C.POINTER(U32))] + [(n, U32) for n in ('plane_id', 'crtc_id', 'fb_id', 'crtc_x', 'crtc_y', 'x', 'y', 'possible_crtcs', 'gamma_size')]
class ObjProps(C.Structure):
    _fields_ = [('count_props', U32), ('props', C.POINTER(U32)), ('prop_values', C.POINTER(C.c_uint64))]
class PropEnum(C.Structure):
    _fields_ = [('value', C.c_uint64), ('name', C.c_char * 32)]
class Property(C.Structure):
    _fields_ = [('prop_id', U32), ('flags', U32), ('name', C.c_char * 32), ('count_values', I), ('values', C.POINTER(C.c_uint64)), ('count_enums', I), ('enums', C.POINTER(PropEnum)), ('count_blobs', I), ('blob_ids', C.POINTER(U32))]

OBJECT_PLANE = 0xeeeeeeee
XRGB8888, ARGB8888 = 0x34325258, 0x34325241

for card in sorted(n for n in os.listdir('/dev/dri') if n.startswith('card')):
    fd = os.open('/dev/dri/' + card, os.O_RDWR)
    r = function('drmModeGetResources', C.POINTER(Resources), [I])(fd)
    if r:
        break
    os.close(fd)
else:
    raise SystemExit('no KMS device')
res = r.contents
assert function('drmSetClientCap', I, [I, C.c_uint64, C.c_uint64])(fd, 2, 1) == 0  # universal planes
conn = function('drmModeGetConnector', C.POINTER(Connector), [I, U32])(fd, res.connectors[0]).contents
assert conn.connection == 1 and conn.count_modes, 'HDMI not connected'
mode = conn.modes[0]
W, H = mode.hdisplay, mode.vdisplay
crtc = res.crtcs[0]
print('KMS device /dev/dri/%s, mode %dx%d, CRTC %d' % (card, W, H, crtc), flush=True)

def buffer(w, h, fourcc, fill):
    """A dumb buffer w x h, 32 bpp, filled by fill(x, y) -> 32-bit pixel; its framebuffer ID."""
    create = bytearray(struct.pack('IIIIIIQ', h, w, 32, 0, 0, 0, 0))
    fcntl.ioctl(fd, 0xc02064b2, create)  # DRM_IOCTL_MODE_CREATE_DUMB
    _, _, _, _, handle, pitch, size = struct.unpack('IIIIIIQ', create)
    req = bytearray(struct.pack('IIQ', handle, 0, 0))
    fcntl.ioctl(fd, 0xc01064b3, req)  # DRM_IOCTL_MODE_MAP_DUMB
    m = mmap.mmap(fd, size, offset=struct.unpack('IIQ', req)[2])
    for y in range(h):
        m[y * pitch:y * pitch + 4 * w] = b''.join(struct.pack('<I', fill(x, y)) for x in range(w)) if w <= 64 else struct.pack('<I', fill(0, y)) * w
    m.close()
    fb = U32()
    handles, pitches, offsets = (U32 * 4)(handle), (U32 * 4)(pitch), (U32 * 4)(0)
    rc = function('drmModeAddFB2', I, [I, U32, U32, U32, C.POINTER(U32), C.POINTER(U32), C.POINTER(U32), C.POINTER(U32), U32])(fd, w, h, fourcc, handles, pitches, offsets, C.byref(fb), 0)
    assert rc == 0, 'drmModeAddFB2 failed: %d' % rc
    return fb.value

def props(obj):
    """name -> (property ID, value, {enum name: value})"""
    p = function('drmModeObjectGetProperties', C.POINTER(ObjProps), [I, U32, U32])(fd, obj, OBJECT_PLANE).contents
    out = {}
    for i in range(p.count_props):
        q = function('drmModeGetProperty', C.POINTER(Property), [I, U32])(fd, p.props[i]).contents
        out[q.name.decode()] = (q.prop_id, p.prop_values[i], {q.enums[k].name.decode(): q.enums[k].value for k in range(q.count_enums)})
    return out

# the primary: white above the middle, grey below
primary = buffer(W, H, XRGB8888, lambda x, y: 0x00ffffff if y < H // 2 else 0x00808080)
connector = U32(conn.connector_id)
rc = function('drmModeSetCrtc', I, [I, U32, U32, U32, U32, C.POINTER(U32), I, C.POINTER(ModeInfo)])(fd, crtc, primary, 0, 0, C.byref(connector), 1, C.byref(mode))
assert rc == 0, 'drmModeSetCrtc failed: %d' % rc

pr = function('drmModeGetPlaneResources', C.POINTER(PlaneRes), [I])(fd).contents
overlays = []
for i in range(pr.count_planes):
    pid = pr.planes[i]
    pl = function('drmModeGetPlane', C.POINTER(Plane), [I, U32])(fd, pid).contents
    pp = props(pid)
    kind = {0: 'overlay', 1: 'primary', 2: 'cursor'}.get(pp['type'][1], '?')
    zpos = pp.get('zpos', (0, -1, {}))[1]
    print('plane %d: %s, zpos %d, CRTCs 0x%x, blend modes %s' % (pid, kind, zpos, pl.possible_crtcs, sorted(pp.get('pixel blend mode', (0, 0, {}))[2])), flush=True)
    if kind == 'overlay' and pl.possible_crtcs & 1:
        overlays.append((zpos, pid, pp))
overlays.sort()
assert len(overlays) >= 4, 'need four overlay planes, have %d' % len(overlays)

set_plane = function('drmModeSetPlane', I, [I, U32, U32, U32, U32, I, I, U32, U32, U32, U32, U32, U32])
def show(plane, fb, x, y, w, h, blend=None):
    zpos, pid, pp = plane
    if blend:
        prop, _, enums = pp['pixel blend mode']
        assert function('drmModeObjectSetProperty', I, [I, U32, U32, U32, C.c_uint64])(fd, pid, OBJECT_PLANE, prop, enums[blend]) == 0
    rc = set_plane(fd, pid, crtc, fb, 0, x, y, w, h, 0, 0, w << 16, h << 16)
    assert rc == 0, 'drmModeSetPlane %d failed: %d' % (pid, rc)
    print('plane %d (zpos %d): %dx%d at (%d, %d)%s' % (pid, zpos, w, h, x, y, ', ' + blend if blend else ''), flush=True)

pw, ph, py = W // 4, H // 3, H // 2 - H // 6
red = buffer(pw, ph, XRGB8888, lambda x, y: 0x00ff0000)
grey = buffer(pw, ph, ARGB8888, lambda x, y: 0x80404040)
# D on the lowest overlay (a Cluster window on rockchipdrm), A, B, C on the top three (Esmart)
show(overlays[0], red, W // 16, H - H // 6 - 20, pw, H // 6)
show(overlays[-3], red, W // 16, py, pw, ph)
show(overlays[-2], grey, W // 2 - pw // 2, py, pw, ph, 'Pre-multiplied')
show(overlays[-1], grey, W - W // 16 - pw, py, pw, ph, 'Coverage')
print('KMS_PLANES_OK %dx%d' % (W, H), flush=True)
time.sleep(float(os.environ.get('KMS_HOLD', '3')))
print('KMS_END', flush=True)
