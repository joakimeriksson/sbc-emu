//! The screen. nouveau draws (through the GPU's render node) into buffers that the display's driver,
//! tegra-drm, shows as they are, tiled: they are shared between the two (PRIME). Mesa can also open the display's card
//! itself and find nouveau behind it, but that driver passes on neither shader storage buffers nor the
//! size of patches (Mesa 25.0).
use crate::sys::*;
use std::ffi::{c_int, CString};
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::ptr::null_mut;

const XRGB8888: u32 = 0x3432_5258; // "XR24"

/// The GPU's render node.
pub struct Gpu {
    gbm: P,
    display: P,
    _file: Option<File>,
}

impl Gpu {
    /// Whichever Mesa chooses (so also llvmpipe), not to be shown.
    pub fn any() -> Result<Self, String> {
        Ok(Gpu { gbm: null_mut(), display: context(0x31dd, null_mut())?, _file: None }) // surfaceless
    }

    pub fn open() -> Result<Self, String> {
        for k in 128..132 {
            let driver = std::fs::read_link(format!("/sys/class/drm/renderD{k}/device/driver")).unwrap_or_default();
            if driver.file_name().is_none_or(|d| d != "nouveau") {
                continue;
            }
            let file = File::options().read(true).write(true).open(format!("/dev/dri/renderD{k}")).map_err(|e| format!("/dev/dri/renderD{k}: {e}"))?;
            // SAFETY: GBM calls with what they gave
            unsafe {
                let gbm = gbm_create_device(file.as_raw_fd());
                if gbm.is_null() {
                    return Err("no GBM on the GPU".into());
                }
                return Ok(Gpu { gbm, display: context(0x31d7, gbm)?, _file: Some(file) });
            }
        }
        Err("no GPU with the nouveau driver".into())
    }
}

/// A display of an EGL platform, with an OpenGL ES 3.2 context, current without a surface.
fn context(platform: u32, native: P) -> Result<P, String> {
    // SAFETY: EGL calls with what they gave
    unsafe {
        let display = eglGetPlatformDisplay(platform, native, null_mut());
        let (mut a, mut b) = (0, 0);
        if display.is_null() || eglInitialize(display, &mut a, &mut b) == 0 {
            return Err("eglInitialize failed".into());
        }
        eglBindAPI(0x30a0); // OpenGL ES
        let attr = [0x3098, 3, 0x30fb, 2, 0x3038]; // version 3.2
        let c = eglCreateContext(display, null_mut(), null_mut(), attr.as_ptr());
        if c.is_null() || eglMakeCurrent(display, null_mut(), null_mut(), c) == 0 {
            return Err("no OpenGL ES 3.2 context".into());
        }
        Ok(display)
    }
}

/// A texture of its own.
fn texture(format: u32, w: u32, h: u32) -> u32 {
    let mut t = 0;
    // SAFETY: plain GL calls
    unsafe {
        glGenTextures(1, &mut t);
        glBindTexture(TEXTURE_2D, t);
        glTexStorage2D(TEXTURE_2D, 1, format, w as i32, h as i32);
    }
    t
}

/// What scenes are drawn into: a colour texture and a depth texture.
fn framebuffer(color: u32, depth: u32) -> u32 {
    let mut fb = 0;
    // SAFETY: plain GL calls
    unsafe {
        glGenFramebuffers(1, &mut fb);
        glBindFramebuffer(FRAMEBUFFER, fb);
        glFramebufferTexture2D(FRAMEBUFFER, COLOR_ATTACHMENT0, TEXTURE_2D, color, 0);
        glFramebufferTexture2D(FRAMEBUFFER, DEPTH_ATTACHMENT, TEXTURE_2D, depth, 0);
    }
    fb
}

pub fn offscreen(w: u32, h: u32) -> u32 {
    framebuffer(texture(RGBA8, w, h), texture(DEPTH24, w, h))
}

/// A buffer both drivers have: GL's framebuffer for it and the display's.
struct Shared {
    gl: u32,
    kms: u32,
}

pub struct Screen {
    file: File,
    crtc: u32,
    connector: u32,
    mode: Mode,
    buffers: Vec<Shared>,
    next: usize,
    /// the mode is set
    on: bool,
    pub width: u32,
    pub height: u32,
}

impl Screen {
    /// The first connected connector of any card; its mode of `want` pixels, or the first.
    pub fn open(gpu: &Gpu, want: Option<(u32, u32)>) -> Result<Self, String> {
        for k in 0..4 {
            let Ok(file) = File::options().read(true).write(true).open(format!("/dev/dri/card{k}")) else { continue };
            let fd = file.as_raw_fd();
            // SAFETY: libdrm's structures are valid until freed, which they are not
            unsafe {
                let Some(res) = drmModeGetResources(fd).as_ref() else { continue };
                for i in 0..res.count_connectors as usize {
                    let Some(c) = drmModeGetConnector(fd, *res.connectors.add(i)).as_ref() else { continue };
                    if c.connection != 1 || c.count_modes == 0 || c.count_encoders == 0 {
                        continue;
                    }
                    let modes = std::slice::from_raw_parts(c.modes, c.count_modes as usize);
                    let fits = |m: &&Mode| want.is_none_or(|w| w == (m.hdisplay as u32, m.vdisplay as u32));
                    let mode = *modes.iter().find(fits).unwrap_or(&modes[0]);
                    let Some(e) = drmModeGetEncoder(fd, *c.encoders).as_ref() else { continue };
                    let Some(n) = (0..res.count_crtcs as usize).find(|n| e.possible_crtcs >> n & 1 != 0) else { continue };
                    let (crtc, connector) = (*res.crtcs.add(n), c.id);
                    let (width, height) = (mode.hdisplay as u32, mode.vdisplay as u32);
                    let mut s = Screen { file, crtc, connector, mode, buffers: Vec::new(), next: 0, on: false, width, height };
                    let depth = texture(DEPTH24, width, height);
                    for _ in 0..2 {
                        let b = s.share(gpu, depth)?;
                        s.buffers.push(b);
                    }
                    return Ok(s);
                }
            }
        }
        Err("no card with a monitor".into())
    }

    /// A buffer of the GPU's that the display can show.
    fn share(&self, gpu: &Gpu, depth: u32) -> Result<Shared, String> {
        let fd = self.file.as_raw_fd();
        let (w, h) = (self.width, self.height);
        // SAFETY: handles, images and textures of the buffer just made
        unsafe {
            let bo = gbm_bo_create(gpu.gbm, w, h, XRGB8888, 4); // rendering
            if bo.is_null() {
                return Err("no buffer".into());
            }
            let proc_ = |name: &str| {
                let n = CString::new(name).unwrap();
                eglGetProcAddress(n.as_ptr())
            };
            let create: unsafe extern "C" fn(P, P, u32, P, *const c_int) -> P = std::mem::transmute(proc_("eglCreateImageKHR"));
            let bind: unsafe extern "C" fn(u32, P) = std::mem::transmute(proc_("glEGLImageTargetTexture2DOES"));
            let image = create(gpu.display, null_mut(), 0x30b0, bo, [0x3038].as_ptr()); // a native pixmap
            if image.is_null() {
                return Err("no image of the buffer".into());
            }
            let mut tex = 0;
            glGenTextures(1, &mut tex);
            glBindTexture(TEXTURE_2D, tex);
            bind(TEXTURE_2D, image);
            let gl = framebuffer(tex, depth);
            if glCheckFramebufferStatus(FRAMEBUFFER) != 0x8cd5 {
                return Err("the shared buffer cannot be drawn into".into());
            }
            let (mut handle, mut kms) = (0, 0);
            if drmPrimeFDToHandle(fd, gbm_bo_get_fd(bo), &mut handle) != 0 {
                return Err("the display does not take the GPU's buffer".into());
            }
            let (handles, pitches) = ([handle, 0, 0, 0], [gbm_bo_get_stride(bo), 0, 0, 0]);
            // block-linear, of 16Bx2 GOBs and a block height: nouveau also says which page kind, which the
            // display's driver does not know
            let modifier = gbm_bo_get_modifier(bo);
            if modifier >> 56 != 3 || modifier & 0x10 == 0 {
                return Err(format!("the GPU's buffer is not block-linear (modifier {modifier:#x})"));
            }
            let modifiers = [3 << 56 | modifier & 0x1f, 0, 0, 0];
            if drmModeAddFB2WithModifiers(fd, w, h, XRGB8888, handles.as_ptr(), pitches.as_ptr(), [0; 4].as_ptr(), modifiers.as_ptr(), &mut kms, 2) != 0 {
                return Err(format!("the display does not show the GPU's buffer (modifier {modifier:#x})"));
            }
            Ok(Shared { gl, kms })
        }
    }

    /// What to draw the next picture into. (GL's pictures begin at the bottom, the display's at the top:
    /// it has to be upside down.)
    pub fn target(&self) -> u32 {
        self.buffers[self.next].gl
    }

    /// Show what was drawn, at the next frame.
    pub fn show(&mut self) -> Result<(), String> {
        let fd = self.file.as_raw_fd();
        let b = &self.buffers[self.next];
        // SAFETY: plain GL and DRM calls
        unsafe {
            glFinish();
            if !self.on {
                if drmModeSetCrtc(fd, self.crtc, b.kms, 0, 0, &mut self.connector, 1, &mut self.mode) != 0 {
                    return Err("cannot set the mode (is something else using the screen?)".into());
                }
                self.on = true;
            } else {
                if drmModePageFlip(fd, self.crtc, b.kms, 1, null_mut()) != 0 {
                    return Err("page flip failed".into());
                }
                // the event that it happened
                let _ = self.file.read(&mut [0; 256]);
            }
        }
        self.next = 1 - self.next;
        Ok(())
    }
}
