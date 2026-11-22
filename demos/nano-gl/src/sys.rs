//! What is used of EGL, OpenGL ES 3.2, GBM and libdrm.
#![allow(non_snake_case, non_upper_case_globals)]
use std::ffi::{c_char, c_int, c_void};

pub type P = *mut c_void;

pub const COLOR_BUFFER_BIT: u32 = 0x4000;
pub const DEPTH_BUFFER_BIT: u32 = 0x100;
pub const TRIANGLES: u32 = 4;
pub const POINTS: u32 = 0;
pub const PATCHES: u32 = 14;
pub const DEPTH_TEST: u32 = 0xb71;
pub const CULL_FACE: u32 = 0xb44;
pub const BLEND: u32 = 0xbe2;
pub const ONE: u32 = 1;
pub const FRAMEBUFFER: u32 = 0x8d40;
pub const READ_FRAMEBUFFER: u32 = 0x8ca8;
pub const DRAW_FRAMEBUFFER: u32 = 0x8ca9;
pub const COLOR_ATTACHMENT0: u32 = 0x8ce0;
pub const DEPTH_ATTACHMENT: u32 = 0x8d00;
pub const TEXTURE_2D: u32 = 0xde1;
pub const RGBA8: u32 = 0x8058;
pub const DEPTH24: u32 = 0x81a6;
pub const LINEAR: i32 = 0x2601;
pub const ARRAY_BUFFER: u32 = 0x8892;
pub const TRANSFORM_FEEDBACK_BUFFER: u32 = 0x8c8e;
pub const RASTERIZER_DISCARD: u32 = 0x8c89;
pub const DYNAMIC_COPY: u32 = 0x88ea;
pub const VERTEX: u32 = 0x8b31;
pub const FRAGMENT: u32 = 0x8b30;
pub const GEOMETRY: u32 = 0x8dd9;
pub const TESS_CONTROL: u32 = 0x8e88;
pub const TESS_EVALUATION: u32 = 0x8e87;

#[link(name = "GLESv2")]
extern "C" {
    pub fn glClearColor(r: f32, g: f32, b: f32, a: f32);
    pub fn glClear(mask: u32);
    pub fn glViewport(x: i32, y: i32, w: i32, h: i32);
    pub fn glEnable(cap: u32);
    pub fn glDisable(cap: u32);
    pub fn glBlendFunc(s: u32, d: u32);
    pub fn glDepthMask(on: u8);
    pub fn glFrontFace(winding: u32);
    pub fn glGetString(name: u32) -> *const c_char;
    pub fn glGetError() -> u32;
    pub fn glCreateShader(ty: u32) -> u32;
    pub fn glShaderSource(s: u32, n: i32, src: *const *const c_char, len: *const i32);
    pub fn glCompileShader(s: u32);
    pub fn glGetShaderiv(s: u32, what: u32, v: *mut i32);
    pub fn glGetShaderInfoLog(s: u32, n: i32, len: *mut i32, log: *mut c_char);
    pub fn glCreateProgram() -> u32;
    pub fn glAttachShader(p: u32, s: u32);
    pub fn glDeleteShader(s: u32);
    pub fn glLinkProgram(p: u32);
    pub fn glGetProgramiv(p: u32, what: u32, v: *mut i32);
    pub fn glGetProgramInfoLog(p: u32, n: i32, len: *mut i32, log: *mut c_char);
    pub fn glUseProgram(p: u32);
    pub fn glGetUniformLocation(p: u32, name: *const c_char) -> i32;
    pub fn glUniform1f(l: i32, v: f32);
    pub fn glUniform2f(l: i32, x: f32, y: f32);
    pub fn glUniform1i(l: i32, v: i32);
    pub fn glUniformMatrix4fv(l: i32, n: i32, transpose: u8, v: *const f32);
    pub fn glDrawArrays(mode: u32, first: i32, n: i32);
    pub fn glDrawArraysInstanced(mode: u32, first: i32, n: i32, instances: i32);
    pub fn glPatchParameteri(what: u32, v: i32);
    pub fn glTransformFeedbackVaryings(p: u32, n: i32, names: *const *const c_char, mode: u32);
    pub fn glBeginTransformFeedback(mode: u32);
    pub fn glEndTransformFeedback();
    pub fn glVertexAttribPointer(a: u32, n: i32, ty: u32, normalized: u8, stride: i32, at: *const c_void);
    pub fn glEnableVertexAttribArray(a: u32);
    pub fn glGenBuffers(n: i32, b: *mut u32);
    pub fn glBindBuffer(t: u32, b: u32);
    pub fn glBindBufferBase(t: u32, i: u32, b: u32);
    pub fn glBufferData(t: u32, n: isize, d: *const c_void, usage: u32);
    pub fn glGenVertexArrays(n: i32, v: *mut u32);
    pub fn glBindVertexArray(v: u32);
    pub fn glGenTextures(n: i32, t: *mut u32);
    pub fn glBindTexture(t: u32, v: u32);
    pub fn glTexStorage2D(t: u32, levels: i32, f: u32, w: i32, h: i32);
    pub fn glTexParameteri(t: u32, what: u32, v: i32);
    pub fn glGenFramebuffers(n: i32, f: *mut u32);
    pub fn glBindFramebuffer(t: u32, f: u32);
    pub fn glFramebufferTexture2D(t: u32, at: u32, tt: u32, tex: u32, level: i32);
    pub fn glBlitFramebuffer(x0: i32, y0: i32, x1: i32, y1: i32, a0: i32, b0: i32, a1: i32, b1: i32, mask: u32, filter: u32);
    pub fn glFinish();
    pub fn glCheckFramebufferStatus(t: u32) -> u32;
    pub fn glReadPixels(x: i32, y: i32, w: i32, h: i32, f: u32, t: u32, d: *mut c_void);
}

#[link(name = "EGL")]
extern "C" {
    pub fn eglGetPlatformDisplay(platform: u32, native: P, attr: *const isize) -> P;
    pub fn eglInitialize(d: P, major: *mut c_int, minor: *mut c_int) -> u32;
    pub fn eglBindAPI(api: u32) -> u32;
    pub fn eglCreateContext(d: P, c: P, share: P, attr: *const c_int) -> P;
    pub fn eglMakeCurrent(d: P, draw: P, read: P, c: P) -> u32;
    pub fn eglGetProcAddress(name: *const c_char) -> P;
}

#[link(name = "gbm")]
extern "C" {
    pub fn gbm_create_device(fd: c_int) -> P;
    pub fn gbm_bo_create(d: P, w: u32, h: u32, f: u32, flags: u32) -> P;
    pub fn gbm_bo_get_fd(bo: P) -> c_int;
    pub fn gbm_bo_get_stride(bo: P) -> u32;
    pub fn gbm_bo_get_modifier(bo: P) -> u64;
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Mode {
    pub clock: u32,
    pub hdisplay: u16,
    pub horizontal: [u16; 4],
    pub vdisplay: u16,
    pub vertical: [u16; 4],
    pub vrefresh: u32,
    pub flags: u32,
    pub ty: u32,
    pub name: [c_char; 32],
}

#[repr(C)]
pub struct Resources {
    pub count_fbs: c_int,
    pub fbs: *mut u32,
    pub count_crtcs: c_int,
    pub crtcs: *mut u32,
    pub count_connectors: c_int,
    pub connectors: *mut u32,
    pub count_encoders: c_int,
    pub encoders: *mut u32,
    pub sizes: [u32; 4],
}

#[repr(C)]
pub struct Connector {
    pub id: u32,
    pub encoder: u32,
    pub ty: u32,
    pub ty_id: u32,
    pub connection: u32,
    pub mm: [u32; 2],
    pub subpixel: u32,
    pub count_modes: c_int,
    pub modes: *mut Mode,
    pub count_props: c_int,
    pub props: *mut u32,
    pub prop_values: *mut u64,
    pub count_encoders: c_int,
    pub encoders: *mut u32,
}

#[repr(C)]
pub struct Encoder {
    pub id: u32,
    pub ty: u32,
    pub crtc: u32,
    pub possible_crtcs: u32,
    pub possible_clones: u32,
}

#[link(name = "drm")]
extern "C" {
    pub fn drmModeGetResources(fd: c_int) -> *mut Resources;
    pub fn drmModeGetConnector(fd: c_int, id: u32) -> *mut Connector;
    pub fn drmModeGetEncoder(fd: c_int, id: u32) -> *mut Encoder;
    pub fn drmModeSetCrtc(fd: c_int, crtc: u32, fb: u32, x: u32, y: u32, connectors: *mut u32, n: c_int, mode: *mut Mode) -> c_int;
    pub fn drmModeAddFB2WithModifiers(fd: c_int, w: u32, h: u32, f: u32, handles: *const u32, pitches: *const u32, offsets: *const u32, modifiers: *const u64, id: *mut u32, flags: u32) -> c_int;
    pub fn drmPrimeFDToHandle(fd: c_int, prime: c_int, handle: *mut u32) -> c_int;
    pub fn drmModePageFlip(fd: c_int, crtc: u32, fb: u32, flags: u32, data: P) -> c_int;
}
