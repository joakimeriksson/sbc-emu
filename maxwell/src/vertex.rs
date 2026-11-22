//! Vertex fetch: attribute formats, streams and index buffers of the 3D class.
use crate::Memory;

/// What an attribute register says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attribute {
    pub stream: usize,
    /// not from a stream: the attribute's constant value
    pub constant: bool,
    pub offset: u32,
    size: u32,
    kind: u32,
    swap_rb: bool,
}

impl Attribute {
    pub fn new(v: u32) -> Self {
        Self { stream: (v & 31) as usize, constant: v >> 6 & 1 != 0, offset: v >> 7 & 0x3fff, size: v >> 21 & 63, kind: v >> 27 & 7, swap_rb: v >> 31 != 0 }
    }

    /// Bytes in memory, and the bits of each component.
    fn layout(&self) -> (usize, &'static [u32]) {
        match self.size {
            0x01 => (16, &[32, 32, 32, 32]),
            0x02 => (12, &[32, 32, 32]),
            0x03 => (8, &[16, 16, 16, 16]),
            0x04 => (8, &[32, 32]),
            0x05 => (6, &[16, 16, 16]),
            0x0a | 0x2f => (4, &[8, 8, 8, 8]),
            0x0f => (4, &[16, 16]),
            0x12 => (4, &[32]),
            0x13 | 0x33 => (if self.size == 0x13 { 3 } else { 4 }, &[8, 8, 8]),
            0x18 | 0x32 => (2, &[8, 8]),
            0x1b => (2, &[16]),
            0x1d | 0x34 => (1, &[8]),
            0x30 => (4, &[10, 10, 10, 2]),
            0x31 => (4, &[11, 11, 10]),
            _ => (0, &[]),
        }
    }

    /// The attribute's four components as the shader reads them.
    pub fn fetch(&self, at: u64, mem: &dyn Memory) -> [u32; 4] {
        let (bytes, bits) = self.layout();
        let mut raw = [0u8; 16];
        mem.read(at, &mut raw[..bytes]);
        let whole = u128::from_le_bytes(raw);
        // integers get 1, the others 1.0 as their fourth component
        let one = if matches!(self.kind, 3 | 4) { 1 } else { 1f32.to_bits() };
        let mut out = [0, 0, 0, one];
        let mut shift = 0;
        for (o, &n) in out.iter_mut().zip(bits) {
            let v = (whole >> shift) as u32 & (u32::MAX >> (32 - n));
            shift += n;
            *o = convert(v, n, self.kind);
        }
        if self.swap_rb {
            out.swap(0, 2);
        }
        out
    }
}

fn convert(v: u32, bits: u32, kind: u32) -> u32 {
    let signed = ((v << (32 - bits)) as i32) >> (32 - bits);
    let max = (1u64 << bits) as f64 - 1.0;
    match kind {
        1 => ((signed as f64 / ((1u64 << (bits - 1)) as f64 - 1.0)).max(-1.0) as f32).to_bits(),
        2 => ((v as f64 / max) as f32).to_bits(),
        3 => signed as u32,
        4 => v,
        5 => (v as f32).to_bits(),
        6 => (signed as f32).to_bits(),
        _ => match bits {
            32 => v,
            16 => half(v as u16).to_bits(),
            11 => small_float(v, 6).to_bits(),
            10 => small_float(v, 5).to_bits(),
            _ => (v as f32).to_bits(),
        },
    }
}

pub fn half(v: u16) -> f32 {
    let (s, e, m) = ((v >> 15) as u32, (v >> 10 & 31) as u32, (v & 0x3ff) as u32);
    let bits = match (e, m) {
        (0, 0) => 0,
        (0, _) => {
            let f = m as f32 / (1 << 24) as f32;
            f.to_bits()
        }
        (31, _) => 0xff << 23 | m << 13,
        _ => (e + 112) << 23 | m << 13,
    };
    f32::from_bits(s << 31 | bits)
}

/// An unsigned float with a 5-bit exponent.
fn small_float(v: u32, mantissa: u32) -> f32 {
    half((v << (10 - mantissa)) as u16 & 0x7fff)
}

/// An index buffer read ahead: `count` indices of `1 << size` bytes from `at`.
pub fn indices(at: u64, size: u32, count: usize, mem: &dyn Memory) -> Vec<u32> {
    let mut raw = vec![0u8; count << size];
    mem.read(at, &mut raw);
    match size {
        0 => raw.iter().map(|&b| b as u32).collect(),
        1 => raw.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]]) as u32).collect(),
        _ => raw.chunks_exact(4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).collect(),
    }
}

/// Primitives of a list of vertices, as indices into it: points, lines or triangles; the provoking
/// vertex is the last of each (or the first).
pub fn assemble(op: u32, n: usize, first_provokes: bool, mut f: impl FnMut(&[u32])) {
    let n = n as u32;
    match op {
        0 => (0..n).for_each(|k| f(&[k])),
        1 => (0..n / 2).for_each(|k| f(&[2 * k, 2 * k + 1])),
        2 | 3 => {
            for k in 1..n {
                f(&[k - 1, k]);
            }
            if op == 2 && n > 2 {
                f(&[n - 1, 0]);
            }
        }
        4 => (0..n / 3).for_each(|k| f(&[3 * k, 3 * k + 1, 3 * k + 2])),
        5 => (2..n).for_each(|k| match (k & 1 == 0, first_provokes) {
            (true, _) => f(&[k - 2, k - 1, k]),
            (false, false) => f(&[k - 1, k - 2, k]),
            (false, true) => f(&[k - 2, k, k - 1]),
        }),
        6 => (2..n).for_each(|k| if first_provokes { f(&[k - 1, k, 0]) } else { f(&[0, k - 1, k]) }),
        9 => (2..n).for_each(|k| f(&[k - 1, k, 0])), // a polygon's first vertex provokes
        7 => (0..n / 4).for_each(|q| {
            let q = 4 * q;
            if first_provokes {
                f(&[q, q + 1, q + 2]);
                f(&[q, q + 2, q + 3]);
            } else {
                f(&[q, q + 1, q + 3]);
                f(&[q + 1, q + 2, q + 3]);
            }
        }),
        8 => (0..n.saturating_sub(2) / 2).for_each(|q| {
            let q = 2 * q;
            f(&[q, q + 1, q + 3]);
            f(&if first_provokes { [q, q + 3, q + 2] } else { [q + 2, q, q + 3] });
        }),
        10 => (0..n / 4).for_each(|k| f(&[4 * k, 4 * k + 1, 4 * k + 2, 4 * k + 3])),
        11 => (3..n).for_each(|k| f(&[k - 3, k - 2, k - 1, k])),
        12 => (0..n / 6).for_each(|k| f(&[6 * k, 6 * k + 1, 6 * k + 2, 6 * k + 3, 6 * k + 4, 6 * k + 5])),
        13 => {
            // triangle strip with adjacency: vertices 0, 2, 4, ... are the strip
            let tris = (n / 2).saturating_sub(2);
            for t in 0..tris {
                let v = 2 * t;
                let before = if t == 0 { 1 } else { v - 2 };
                let after = if t + 1 == tris { v + 5 } else { v + 6 };
                if t & 1 == 0 {
                    f(&[v, before, v + 2, after, v + 4, v + 3]);
                } else {
                    f(&[v + 2, before, v, v + 3, v + 4, after]);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Ram(Vec<u8>);
    impl Memory for Ram {
        fn read(&self, va: u64, out: &mut [u8]) { out.copy_from_slice(&self.0[va as usize..va as usize + out.len()]) }
        fn write(&mut self, _: u64, _: &[u8]) {}
    }

    fn attribute(size: u32, kind: u32) -> Attribute {
        Attribute::new(size << 21 | kind << 27)
    }

    #[test]
    fn formats() {
        let f = |v: [u32; 4]| v.map(f32::from_bits);
        let r = Ram(vec![0xff, 0x80, 0x00, 0x7f, 0, 0x3c, 0, 0xc0]);
        assert_eq!(f(attribute(0x0a, 2).fetch(0, &r)), [1.0, 128.0 / 255.0, 0.0, 127.0 / 255.0]);
        assert_eq!(f(attribute(0x0a, 1).fetch(0, &r)), [-1.0 / 127.0, -1.0, 0.0, 1.0]);
        assert_eq!(attribute(0x18, 3).fetch(0, &r), [-1i32 as u32, -128i32 as u32, 0, 1]);
        assert_eq!(f(attribute(0x0f, 7).fetch(4, &r)), [1.0, -2.0, 0.0, 1.0]);
        assert_eq!(f(attribute(0x18, 5).fetch(0, &r)), [255.0, 128.0, 0.0, 1.0]);
        let a = Attribute::new(3 | 1 << 6 | 24 << 7 | 0x0a << 21 | 2 << 27 | 1 << 31);
        assert_eq!((a.stream, a.constant, a.offset), (3, true, 24));
        assert_eq!(f(a.fetch(0, &r)), [0.0, 128.0 / 255.0, 1.0, 127.0 / 255.0]);
        let packed = Ram((0x3ffu32 | 0x200 << 10 | 3 << 30).to_le_bytes().to_vec());
        assert_eq!(f(attribute(0x30, 2).fetch(0, &packed)), [1.0, 512.0 / 1023.0, 0.0, 1.0]);
        // 11 bits: exponent 15 is 1.0
        let r11 = Ram((15u32 << 6 | (16u32 << 6) << 11 | (14u32 << 5) << 22).to_le_bytes().to_vec());
        assert_eq!(f(attribute(0x31, 7).fetch(0, &r11)), [1.0, 2.0, 0.5, 1.0]);
        assert_eq!((half(0x0001), half(0x7c00)), (2f32.powi(-24), f32::INFINITY));
    }

    #[test]
    fn strips_keep_their_winding() {
        let all = |op, n, first| { let mut v = Vec::new(); assemble(op, n, first, |p| v.push(p.to_vec())); v };
        assert_eq!(all(5, 4, false), [[0, 1, 2], [2, 1, 3]]);
        assert_eq!(all(5, 4, true), [[0, 1, 2], [1, 3, 2]]);
        assert_eq!(all(6, 4, false), [[0, 1, 2], [0, 2, 3]]);
        assert_eq!(all(2, 3, false), [[0, 1], [1, 2], [2, 0]]);
        assert_eq!(all(7, 4, false).len(), 2);
        assert_eq!(all(13, 6, false), [[0, 1, 2, 5, 4, 3]]);
        assert_eq!(all(13, 8, false), [[0, 1, 2, 6, 4, 3], [4, 0, 2, 5, 6, 7]]);
    }
}
