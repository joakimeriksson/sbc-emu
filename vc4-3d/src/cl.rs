//! Control-list packets (Mesa's `vc4_packet.xml`, the kernel's `vc4_packet.h`), the GL state
//! they carry, shader records, and primitive assembly.

use crate::mem::Mem;

pub const HALT: u8 = 0;
pub const NOP: u8 = 1;
pub const FLUSH: u8 = 4;
pub const FLUSH_ALL: u8 = 5;
pub const START_TILE_BINNING: u8 = 6;
pub const INCREMENT_SEMAPHORE: u8 = 7;
pub const WAIT_ON_SEMAPHORE: u8 = 8;
pub const BRANCH: u8 = 16;
pub const BRANCH_TO_SUB_LIST: u8 = 17;
pub const RETURN_FROM_SUB_LIST: u8 = 18;
pub const STORE_MS_TILE_BUFFER: u8 = 24;
pub const STORE_MS_TILE_BUFFER_AND_EOF: u8 = 25;
pub const STORE_FULL_RES_TILE_BUFFER: u8 = 26;
pub const LOAD_FULL_RES_TILE_BUFFER: u8 = 27;
pub const STORE_TILE_BUFFER_GENERAL: u8 = 28;
pub const LOAD_TILE_BUFFER_GENERAL: u8 = 29;
pub const GL_INDEXED_PRIMITIVE: u8 = 32;
pub const GL_ARRAY_PRIMITIVE: u8 = 33;
/// In tile lists: a primitive by its vertex indices (the binner's own encoding, see `bin`).
pub const COMPRESSED_PRIMITIVE: u8 = 48;
/// In tile lists: a primitive the binner clipped, with its new vertices.
pub const CLIPPED_PRIMITIVE: u8 = 49;
pub const PRIMITIVE_LIST_FORMAT: u8 = 56;
pub const GL_SHADER_STATE: u8 = 64;
pub const NV_SHADER_STATE: u8 = 65;
pub const VG_SHADER_STATE: u8 = 66;
pub const CONFIGURATION_BITS: u8 = 96;
pub const FLAT_SHADE_FLAGS: u8 = 97;
pub const POINT_SIZE: u8 = 98;
pub const LINE_WIDTH: u8 = 99;
pub const RHT_X_BOUNDARY: u8 = 100;
pub const DEPTH_OFFSET: u8 = 101;
pub const CLIP_WINDOW: u8 = 102;
pub const VIEWPORT_OFFSET: u8 = 103;
pub const Z_CLIPPING: u8 = 104;
pub const CLIPPER_XY_SCALING: u8 = 105;
pub const CLIPPER_Z_SCALING: u8 = 106;
pub const TILE_BINNING_MODE_CONFIG: u8 = 112;
pub const TILE_RENDERING_MODE_CONFIG: u8 = 113;
pub const CLEAR_COLORS: u8 = 114;
pub const TILE_COORDINATES: u8 = 115;

/// Size of a packet including its opcode byte, or None for an unknown opcode. The two
/// primitive records in tile lists have variable sizes (see `bin`).
pub fn size(op: u8) -> Option<u32> {
    Some(match op {
        HALT | NOP | FLUSH | FLUSH_ALL | START_TILE_BINNING | INCREMENT_SEMAPHORE | WAIT_ON_SEMAPHORE | RETURN_FROM_SUB_LIST
        | STORE_MS_TILE_BUFFER | STORE_MS_TILE_BUFFER_AND_EOF => 1,
        BRANCH | BRANCH_TO_SUB_LIST | STORE_FULL_RES_TILE_BUFFER | LOAD_FULL_RES_TILE_BUFFER => 5,
        STORE_TILE_BUFFER_GENERAL | LOAD_TILE_BUFFER_GENERAL => 7,
        GL_INDEXED_PRIMITIVE => 14,
        GL_ARRAY_PRIMITIVE => 10,
        PRIMITIVE_LIST_FORMAT => 2,
        GL_SHADER_STATE | NV_SHADER_STATE | VG_SHADER_STATE => 5,
        CONFIGURATION_BITS => 4,
        FLAT_SHADE_FLAGS | POINT_SIZE | LINE_WIDTH | DEPTH_OFFSET | VIEWPORT_OFFSET => 5,
        RHT_X_BOUNDARY | TILE_COORDINATES => 3,
        CLIP_WINDOW | Z_CLIPPING | CLIPPER_XY_SCALING | CLIPPER_Z_SCALING => 9,
        TILE_BINNING_MODE_CONFIG => 16,
        TILE_RENDERING_MODE_CONFIG => 11,
        CLEAR_COLORS => 14,
        _ => return None,
    })
}

pub fn name(op: u8) -> &'static str {
    match op {
        HALT => "HALT", NOP => "NOP", FLUSH => "FLUSH", FLUSH_ALL => "FLUSH_ALL", START_TILE_BINNING => "START_TILE_BINNING",
        INCREMENT_SEMAPHORE => "INCREMENT_SEMAPHORE", WAIT_ON_SEMAPHORE => "WAIT_ON_SEMAPHORE", BRANCH => "BRANCH",
        BRANCH_TO_SUB_LIST => "BRANCH_TO_SUB_LIST", RETURN_FROM_SUB_LIST => "RETURN_FROM_SUB_LIST",
        STORE_MS_TILE_BUFFER => "STORE_MS_TILE_BUFFER", STORE_MS_TILE_BUFFER_AND_EOF => "STORE_MS_TILE_BUFFER_AND_EOF",
        STORE_FULL_RES_TILE_BUFFER => "STORE_FULL_RES_TILE_BUFFER", LOAD_FULL_RES_TILE_BUFFER => "LOAD_FULL_RES_TILE_BUFFER",
        STORE_TILE_BUFFER_GENERAL => "STORE_TILE_BUFFER_GENERAL", LOAD_TILE_BUFFER_GENERAL => "LOAD_TILE_BUFFER_GENERAL",
        GL_INDEXED_PRIMITIVE => "GL_INDEXED_PRIMITIVE", GL_ARRAY_PRIMITIVE => "GL_ARRAY_PRIMITIVE",
        COMPRESSED_PRIMITIVE => "PRIMITIVE", CLIPPED_PRIMITIVE => "CLIPPED_PRIMITIVE", PRIMITIVE_LIST_FORMAT => "PRIMITIVE_LIST_FORMAT",
        GL_SHADER_STATE => "GL_SHADER_STATE", NV_SHADER_STATE => "NV_SHADER_STATE", VG_SHADER_STATE => "VG_SHADER_STATE",
        CONFIGURATION_BITS => "CONFIGURATION_BITS", FLAT_SHADE_FLAGS => "FLAT_SHADE_FLAGS", POINT_SIZE => "POINT_SIZE",
        LINE_WIDTH => "LINE_WIDTH", RHT_X_BOUNDARY => "RHT_X_BOUNDARY", DEPTH_OFFSET => "DEPTH_OFFSET", CLIP_WINDOW => "CLIP_WINDOW",
        VIEWPORT_OFFSET => "VIEWPORT_OFFSET", Z_CLIPPING => "Z_CLIPPING", CLIPPER_XY_SCALING => "CLIPPER_XY_SCALING",
        CLIPPER_Z_SCALING => "CLIPPER_Z_SCALING", TILE_BINNING_MODE_CONFIG => "TILE_BINNING_MODE_CONFIG",
        TILE_RENDERING_MODE_CONFIG => "TILE_RENDERING_MODE_CONFIG", CLEAR_COLORS => "CLEAR_COLORS", TILE_COORDINATES => "TILE_COORDINATES",
        _ => "?",
    }
}

#[inline]
pub fn le32(b: &[u8]) -> u32 { u32::from_le_bytes([b[0], b[1], b[2], b[3]]) }
#[inline]
pub fn le16(b: &[u8]) -> u16 { u16::from_le_bytes([b[0], b[1]]) }
#[inline]
pub fn lef(b: &[u8]) -> f32 { f32::from_bits(le32(b)) }

/// Configuration bits (byte 0..2 of the packet).
pub mod config {
    pub const ENABLE_FRONT: u32 = 1 << 0;
    pub const ENABLE_BACK: u32 = 1 << 1;
    pub const CW: u32 = 1 << 2;
    pub const DEPTH_OFFSET: u32 = 1 << 3;
    pub const OVERSAMPLE_4X: u32 = 1 << 6;
    pub const Z_UPDATE: u32 = 1 << 15;
    pub const EARLY_Z: u32 = 1 << 16;
    pub const EARLY_Z_UPDATE: u32 = 1 << 17;
    pub fn depth_func(c: u32) -> u32 { c >> 12 & 7 }
}

/// The state packets both threads use (binning reads them from the binning list; the binner
/// copies what rendering needs into the tile lists).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GlState {
    pub config: u32,
    pub flat: u32,
    pub point_size: f32,
    pub line_width: f32,
    /// depth offset factor and units (float 1-8-7: the top 16 bits of a float)
    pub offset_factor: f32,
    pub offset_units: f32,
    /// clip window: left, bottom (top in memory order), width, height in pixels
    pub clip: [u16; 4],
    /// viewport centre in 1/16 pixels
    pub vp: [i16; 2],
    pub zclip: [f32; 2],
    /// viewport half width/height in 1/16 pixels (clipper)
    pub clip_xy: [f32; 2],
    /// Zc to Zs scale and offset (clipper)
    pub clip_z: [f32; 2],
    /// shader state packet: (opcode, its 32-bit word)
    pub shader: (u8, u32),
    pub prim_format: u8,
}

impl Default for GlState {
    fn default() -> Self {
        GlState { config: config::ENABLE_FRONT | config::ENABLE_BACK | 7 << 12, flat: 0, point_size: 1.0, line_width: 1.0, offset_factor: 0.0, offset_units: 0.0,
                  clip: [0, 0, 4096, 4096], vp: [0, 0], zclip: [0.0, 1.0], clip_xy: [0.0, 0.0], clip_z: [0.5, 0.5], shader: (0, 0), prim_format: 0 }
    }
}

pub fn f187(v: u16) -> f32 { f32::from_bits((v as u32) << 16) }

impl GlState {
    /// Apply a state packet (`p` holds the packet, opcode first). Returns false if `p` is not one.
    pub fn apply(&mut self, p: &[u8]) -> bool {
        match p[0] {
            CONFIGURATION_BITS => self.config = p[1] as u32 | (p[2] as u32) << 8 | (p[3] as u32) << 16,
            FLAT_SHADE_FLAGS => self.flat = le32(&p[1..]),
            POINT_SIZE => self.point_size = lef(&p[1..]),
            LINE_WIDTH => self.line_width = lef(&p[1..]),
            DEPTH_OFFSET => { self.offset_factor = f187(le16(&p[1..])); self.offset_units = f187(le16(&p[3..])); }
            CLIP_WINDOW => self.clip = [le16(&p[1..]), le16(&p[3..]), le16(&p[5..]), le16(&p[7..])],
            VIEWPORT_OFFSET => self.vp = [le16(&p[1..]) as i16, le16(&p[3..]) as i16],
            Z_CLIPPING => self.zclip = [lef(&p[1..]), lef(&p[5..])],
            CLIPPER_XY_SCALING => self.clip_xy = [lef(&p[1..]), lef(&p[5..])],
            CLIPPER_Z_SCALING => self.clip_z = [lef(&p[1..]), lef(&p[5..])],
            GL_SHADER_STATE | NV_SHADER_STATE | VG_SHADER_STATE => self.shader = (p[0], le32(&p[1..])),
            PRIMITIVE_LIST_FORMAT => self.prim_format = p[1],
            RHT_X_BOUNDARY => {}
            _ => return false,
        }
        true
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Attr {
    pub addr: u32,
    pub nbytes: u32,
    pub stride: u32,
    pub vs_off: u32,
    pub cs_off: u32,
}

pub const FLAG_FS_SINGLE_THREAD: u16 = 1;
pub const FLAG_POINT_SIZE: u16 = 2;
pub const FLAG_CLIPPING: u16 = 4;

/// A GL shader record (or an NV one: pre-shaded vertices, no vertex or coordinate shader).
#[derive(Clone, Debug, Default)]
pub struct ShaderRec {
    pub addr: u32,
    pub flags: u16,
    pub fs_nvary: u32,
    pub fs_code: u32,
    pub fs_unif: u32,
    pub vs_sel: u8,
    pub vs_size: u32,
    pub vs_code: u32,
    pub vs_unif: u32,
    pub cs_sel: u8,
    pub cs_size: u32,
    pub cs_code: u32,
    pub cs_unif: u32,
    pub attrs: Vec<Attr>,
    /// NV shader state: shaded vertex data and its stride
    pub nv: Option<(u32, u32)>,
}

impl ShaderRec {
    /// Read the record a shader state packet (`op`, its word) points at.
    pub fn read(op: u8, word: u32, mem: &mut Mem) -> ShaderRec {
        let addr = word & !0xf;
        let mut b = [0u8; 132];
        if op == NV_SHADER_STATE {
            mem.read(addr, &mut b[..16]);
            return ShaderRec { addr, flags: b[0] as u16, fs_nvary: b[3] as u32, fs_code: le32(&b[4..]), fs_unif: le32(&b[8..]),
                               nv: Some((le32(&b[12..]), b[1] as u32)), ..Default::default() };
        }
        if op == VG_SHADER_STATE { crate::log_first("vc4: VG shader state not modelled"); }
        let n = match word & 7 { 0 => 8, n => n } as usize;
        let ext = word & 8 != 0;
        mem.read(addr, &mut b[..if ext { 100 + 4 * n } else { 36 + 8 * n }]);
        let mut r = ShaderRec {
            addr, flags: le16(&b), fs_nvary: b[3] as u32, fs_code: le32(&b[4..]), fs_unif: le32(&b[8..]),
            vs_sel: b[14], vs_size: b[15] as u32, vs_code: le32(&b[16..]), vs_unif: le32(&b[20..]),
            cs_sel: b[26], cs_size: b[27] as u32, cs_code: le32(&b[28..]), cs_unif: le32(&b[32..]),
            attrs: Vec::with_capacity(n), nv: None,
        };
        for i in 0..n {
            let o = 36 + 8 * i;
            let mut stride = b[o + 5] as u32;
            if ext { stride |= le32(&b[100 + 4 * i..]) & !0xff; }
            r.attrs.push(Attr { addr: le32(&b[o..]), nbytes: b[o + 4] as u32 + 1, stride, vs_off: b[o + 6] as u32, cs_off: b[o + 7] as u32 });
        }
        r
    }

    pub fn point_size(&self) -> bool { self.flags & FLAG_POINT_SIZE != 0 }
}

/// Primitive modes of the GL primitive packets.
pub const PRIM_POINTS: u8 = 0;
pub const PRIM_LINES: u8 = 1;
pub const PRIM_LINE_LOOP: u8 = 2;
pub const PRIM_LINE_STRIP: u8 = 3;
pub const PRIM_TRIANGLES: u8 = 4;
pub const PRIM_TRIANGLE_STRIP: u8 = 5;
pub const PRIM_TRIANGLE_FAN: u8 = 6;

/// Split a vertex sequence into primitives (positions in `v`): points, lines or triangles, with
/// the winding of strips kept and the provoking (last) vertex last.
pub fn assemble(mode: u8, n: usize, mut out: impl FnMut(&[usize])) {
    match mode {
        PRIM_POINTS => for i in 0..n { out(&[i]) },
        PRIM_LINES => for i in (0..n.saturating_sub(1)).step_by(2) { out(&[i, i + 1]) },
        PRIM_LINE_LOOP => { if n >= 2 { for i in 0..n - 1 { out(&[i, i + 1]) } out(&[n - 1, 0]) } }
        PRIM_LINE_STRIP => for i in 0..n.saturating_sub(1) { out(&[i, i + 1]) },
        PRIM_TRIANGLES => { let mut i = 0; while i + 2 < n { out(&[i, i + 1, i + 2]); i += 3; } }
        PRIM_TRIANGLE_STRIP => for i in 0..n.saturating_sub(2) { if i & 1 == 0 { out(&[i, i + 1, i + 2]) } else { out(&[i + 1, i, i + 2]) } },
        PRIM_TRIANGLE_FAN => for i in 1..n.saturating_sub(1) { out(&[0, i, i + 1]) },
        _ => { crate::log_first(&format!("vc4: primitive mode {mode} not modelled")); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_keep_winding() {
        let mut v = Vec::new();
        assemble(PRIM_TRIANGLE_STRIP, 5, |p| v.push(p.to_vec()));
        assert_eq!(v, vec![vec![0, 1, 2], vec![2, 1, 3], vec![2, 3, 4]]);
        let mut v = Vec::new();
        assemble(PRIM_LINE_LOOP, 3, |p| v.push(p.to_vec()));
        assert_eq!(v, vec![vec![0, 1], vec![1, 2], vec![2, 0]]);
    }

    #[test]
    fn shader_record_layout() {
        let mut ram = vec![0u8; 0x2000];
        let rec: [u32; 11] = [0x0005_0004, 0x1000, 0x1100, 0x0c03_0000, 0x1200, 0x1300, 0x0803_0000, 0x1400, 0x1500, 0x8000, 0x0004_100f];
        for (k, w) in rec.iter().enumerate() { ram[0x100 + 4 * k..0x104 + 4 * k].copy_from_slice(&w.to_le_bytes()); }
        let mut m = Mem::new(&mut ram);
        let r = ShaderRec::read(GL_SHADER_STATE, 0x101, &mut m);
        assert_eq!(r.flags, 4);
        assert_eq!(r.fs_nvary, 0);
        assert_eq!(r.fs_code, 0x1000);
        assert_eq!((r.vs_sel, r.vs_size, r.vs_code, r.vs_unif), (3, 12, 0x1200, 0x1300));
        assert_eq!((r.cs_sel, r.cs_size, r.cs_code, r.cs_unif), (3, 8, 0x1400, 0x1500));
        assert_eq!(r.attrs.len(), 1);
        let a = r.attrs[0];
        assert_eq!((a.addr, a.nbytes, a.stride, a.vs_off, a.cs_off), (0x8000, 16, 16, 4, 0));
    }
}
