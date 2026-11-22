//! Shaders and matrices.
use crate::sys::*;
use std::ffi::CString;

const HEAD: &str = "#version 320 es\nprecision highp float; precision highp int; precision highp sampler2DShadow;\n";

/// A program of shaders, by type.
pub fn program(parts: &[(u32, &str)]) -> u32 {
    link(parts, &[])
}

/// A vertex shader whose outputs transform feedback captures.
pub fn captured(src: &str, outputs: &[&str]) -> u32 {
    link(&[(VERTEX, src), (FRAGMENT, "out vec4 o; void main() { o = vec4(0); }")], outputs)
}

fn link(parts: &[(u32, &str)], outputs: &[&str]) -> u32 {
    // SAFETY: GL calls on objects made here
    unsafe {
        let p = glCreateProgram();
        for &(ty, src) in parts {
            let s = glCreateShader(ty);
            let text = CString::new(format!("{HEAD}{src}")).unwrap();
            glShaderSource(s, 1, &text.as_ptr(), std::ptr::null());
            glCompileShader(s);
            let mut ok = 0;
            glGetShaderiv(s, 0x8b81, &mut ok);
            if ok == 0 {
                let mut log = vec![0; 4096];
                glGetShaderInfoLog(s, 4096, std::ptr::null_mut(), log.as_mut_ptr());
                panic!("{}\n{src}", std::ffi::CStr::from_ptr(log.as_ptr()).to_string_lossy());
            }
            glAttachShader(p, s);
            glDeleteShader(s); // (with the program)
        }
        let names: Vec<CString> = outputs.iter().map(|o| CString::new(*o).unwrap()).collect();
        let pointers: Vec<_> = names.iter().map(|n| n.as_ptr()).collect();
        if !names.is_empty() {
            glTransformFeedbackVaryings(p, names.len() as i32, pointers.as_ptr(), 0x8c8c); // interleaved
        }
        glLinkProgram(p);
        let mut ok = 0;
        glGetProgramiv(p, 0x8b82, &mut ok);
        if ok == 0 {
            let mut log = vec![0; 4096];
            glGetProgramInfoLog(p, 4096, std::ptr::null_mut(), log.as_mut_ptr());
            panic!("{}", std::ffi::CStr::from_ptr(log.as_ptr()).to_string_lossy());
        }
        glUseProgram(p);
        p
    }
}

pub fn location(p: u32, name: &str) -> i32 {
    let n = CString::new(name).unwrap();
    // SAFETY: a name that ends
    unsafe { glGetUniformLocation(p, n.as_ptr()) }
}

pub fn set(p: u32, name: &str, v: f32) {
    // SAFETY: the program in use
    unsafe { glUniform1f(location(p, name), v) }
}

pub fn set_matrix(p: u32, name: &str, m: &M) {
    // SAFETY: sixteen floats
    unsafe { glUniformMatrix4fv(location(p, name), 1, 0, m.0.as_ptr()) }
}

/// A matrix, column by column.
#[derive(Clone, Copy)]
pub struct M(pub [f32; 16]);

impl std::ops::Mul for M {
    type Output = M;
    fn mul(self, o: M) -> M {
        M(std::array::from_fn(|i| (0..4).map(|k| self.0[k * 4 + i % 4] * o.0[i / 4 * 4 + k]).sum()))
    }
}

impl M {
    pub fn perspective(fov: f32, aspect: f32, near: f32, far: f32) -> M {
        let f = 1.0 / (fov / 2.0).tan();
        let d = near - far;
        M([f / aspect, 0., 0., 0., 0., f, 0., 0., 0., 0., (far + near) / d, -1., 0., 0., 2. * far * near / d, 0.])
    }
    pub fn ortho(s: f32, near: f32, far: f32) -> M {
        let d = near - far;
        M([1. / s, 0., 0., 0., 0., 1. / s, 0., 0., 0., 0., 2. / d, 0., 0., 0., (far + near) / d, 1.])
    }
    pub fn look(e: [f32; 3], c: [f32; 3]) -> M {
        let sub = |a: [f32; 3], b: [f32; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
        let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        let cross = |a: [f32; 3], b: [f32; 3]| [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
        let unit = |a: [f32; 3]| {
            let l = dot(a, a).sqrt();
            [a[0] / l, a[1] / l, a[2] / l]
        };
        let f = unit(sub(c, e));
        let s = unit(cross(f, [0., 1., 0.]));
        let u = cross(s, f);
        M([s[0], u[0], -f[0], 0., s[1], u[1], -f[1], 0., s[2], u[2], -f[2], 0., -dot(s, e), -dot(u, e), dot(f, e), 1.])
    }
    /// A turn about y, then x.
    pub fn turn(y: f32, x: f32) -> M {
        let (s, c) = y.sin_cos();
        let (t, d) = x.sin_cos();
        M([1., 0., 0., 0., 0., d, t, 0., 0., -t, d, 0., 0., 0., 0., 1.]) * M([c, 0., -s, 0., 0., 1., 0., 0., s, 0., c, 0., 0., 0., 0., 1.])
    }
    pub fn at(x: f32, y: f32, z: f32) -> M {
        M([1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., x, y, z, 1.])
    }
}
