//! A baseline JPEG encoder for the USB webcam's MJPEG frames: YUYV (4:2:2, video range) in,
//! JFIF 4:2:2 out, with the example quantisation and Huffman tables of ITU T.81 Annex K. Pure
//! Rust, so it runs in the browser build too; no decoder.

const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21,
    28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61,
    54, 47, 55, 62, 63,
];

// Annex K.1, in natural (row-major) order
const LUMA_Q: [u8; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56, 14, 17, 22, 29, 51,
    87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113, 92, 49, 64, 78, 87, 103, 121, 120, 101,
    72, 92, 95, 98, 112, 100, 103, 99,
];
const CHROMA_Q: [u8; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99, 47, 66, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99,
];

// Annex K.3: code lengths (counts of codes of 1..16 bits) and values
const DC_LUMA_BITS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const DC_CHROMA_BITS: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
const DC_VALS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const AC_LUMA_BITS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
const AC_LUMA_VALS: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14,
    0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09,
    0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a,
    0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65,
    0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88,
    0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9,
    0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca,
    0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea,
    0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
];
const AC_CHROMA_BITS: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
const AC_CHROMA_VALS: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71, 0x13, 0x22, 0x32,
    0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0, 0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16,
    0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39,
    0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64,
    0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86,
    0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
    0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8,
    0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9,
    0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
];

/// A Huffman table as (code, length) per symbol.
struct Huff([(u16, u8); 256]);

impl Huff {
    /// The canonical codes of T.81 Annex C.
    fn new(bits: &[u8; 16], vals: &[u8]) -> Huff {
        let mut t = [(0u16, 0u8); 256];
        let (mut code, mut k) = (0u16, 0usize);
        for (len, &n) in bits.iter().enumerate() {
            for _ in 0..n {
                t[vals[k] as usize] = (code, len as u8 + 1);
                code += 1;
                k += 1;
            }
            code <<= 1;
        }
        Huff(t)
    }
}

struct Bits { out: Vec<u8>, acc: u32, n: u32 }

impl Bits {
    fn put(&mut self, code: u32, len: u32) {
        self.acc = (self.acc << len) | (code & ((1 << len) - 1));
        self.n += len;
        while self.n >= 8 {
            let b = (self.acc >> (self.n - 8)) as u8;
            self.out.push(b);
            if b == 0xff { self.out.push(0); } // byte stuffing
            self.n -= 8;
        }
        self.acc &= (1 << self.n) - 1;
    }
    fn flush(&mut self) { if self.n > 0 { let pad = 8 - self.n; self.put((1 << pad) - 1, pad); } }
}

/// The magnitude category and its bits (T.81 F.1.2.1).
fn category(v: i32) -> (u32, u32) {
    let a = v.unsigned_abs();
    let size = 32 - a.leading_zeros();
    let bits = if v < 0 { (v - 1) as u32 } else { v as u32 };
    (size, bits & ((1u32 << size) - 1))
}

/// An encoder for one picture size and quality; the tables are built once.
pub struct Encoder {
    qt: [[u8; 64]; 2],
    /// 1 / (divisor) per coefficient, the DCT's scale folded in
    div: [[f32; 64]; 2],
    dc: [Huff; 2],
    ac: [Huff; 2],
    cos: [[f32; 8]; 8],
}

impl Encoder {
    /// `quality` 1..=100, as libjpeg's.
    pub fn new(quality: u8) -> Self {
        let q = quality.clamp(1, 100) as u32;
        let scale = if q < 50 { 5000 / q } else { 200 - 2 * q };
        let table = |base: &[u8; 64]| {
            let mut t = [0u8; 64];
            for (o, &b) in t.iter_mut().zip(base) { *o = ((b as u32 * scale + 50) / 100).clamp(1, 255) as u8; }
            t
        };
        let qt = [table(&LUMA_Q), table(&CHROMA_Q)];
        let mut cos = [[0f32; 8]; 8];
        for (u, row) in cos.iter_mut().enumerate() {
            let c = if u == 0 { std::f32::consts::FRAC_1_SQRT_2 } else { 1.0 };
            for (x, v) in row.iter_mut().enumerate() {
                *v = 0.5 * c * (((2 * x + 1) as f32 * u as f32 * std::f32::consts::PI) / 16.0).cos();
            }
        }
        let mut div = [[0f32; 64]; 2];
        for t in 0..2 { for k in 0..64 { div[t][k] = 1.0 / qt[t][k] as f32; } }
        Encoder {
            qt,
            div,
            dc: [Huff::new(&DC_LUMA_BITS, &DC_VALS), Huff::new(&DC_CHROMA_BITS, &DC_VALS)],
            ac: [Huff::new(&AC_LUMA_BITS, &AC_LUMA_VALS), Huff::new(&AC_CHROMA_BITS, &AC_CHROMA_VALS)],
            cos,
        }
    }

    fn headers(&self, w: usize, h: usize, out: &mut Vec<u8>) {
        out.extend_from_slice(&[0xff, 0xd8]);
        out.extend_from_slice(&[0xff, 0xe0, 0, 16, b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0, 1, 0, 0]);
        for (id, t) in self.qt.iter().enumerate() {
            out.extend_from_slice(&[0xff, 0xdb, 0, 67, id as u8]);
            for &z in &ZIGZAG { out.push(t[z]); }
        }
        // SOF0: 8-bit, 3 components: Y 2x1 (4:2:2), Cb and Cr 1x1
        out.extend_from_slice(&[0xff, 0xc0, 0, 17, 8]);
        out.extend_from_slice(&(h as u16).to_be_bytes());
        out.extend_from_slice(&(w as u16).to_be_bytes());
        out.extend_from_slice(&[3, 1, 0x21, 0, 2, 0x11, 1, 3, 0x11, 1]);
        for (class_id, bits, vals) in [
            (0x00, &DC_LUMA_BITS, &DC_VALS[..]),
            (0x10, &AC_LUMA_BITS, &AC_LUMA_VALS[..]),
            (0x01, &DC_CHROMA_BITS, &DC_VALS[..]),
            (0x11, &AC_CHROMA_BITS, &AC_CHROMA_VALS[..]),
        ] {
            let len = 2 + 1 + 16 + vals.len();
            out.extend_from_slice(&[0xff, 0xc4]);
            out.extend_from_slice(&(len as u16).to_be_bytes());
            out.push(class_id);
            out.extend_from_slice(bits);
            out.extend_from_slice(vals);
        }
        out.extend_from_slice(&[0xff, 0xda, 0, 12, 3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0]);
    }

    /// One 8x8 block (samples level-shifted, full range): forward DCT, quantise, entropy-code.
    fn block(&self, s: &[f32; 64], t: usize, pred: &mut i32, bits: &mut Bits) {
        // separable DCT: rows, then columns
        let mut tmp = [0f32; 64];
        for y in 0..8 {
            for u in 0..8 {
                let c = &self.cos[u];
                let r = &s[y * 8..y * 8 + 8];
                tmp[y * 8 + u] = c[0] * r[0] + c[1] * r[1] + c[2] * r[2] + c[3] * r[3] + c[4] * r[4] + c[5] * r[5] + c[6] * r[6] + c[7] * r[7];
            }
        }
        let mut q = [0i32; 64];
        for v in 0..8 {
            let c = &self.cos[v];
            for u in 0..8 {
                let mut a = 0f32;
                for y in 0..8 { a += c[y] * tmp[y * 8 + u]; }
                q[v * 8 + u] = (a * self.div[t][v * 8 + u]).round() as i32;
            }
        }
        let (dc, ac) = (&self.dc[t].0, &self.ac[t].0);
        let diff = q[0] - *pred;
        *pred = q[0];
        let (size, b) = category(diff);
        bits.put(dc[size as usize].0 as u32, dc[size as usize].1 as u32);
        if size > 0 { bits.put(b, size); }
        let mut run = 0;
        for &z in &ZIGZAG[1..] {
            let v = q[z];
            if v == 0 { run += 1; continue; }
            while run > 15 { bits.put(ac[0xf0].0 as u32, ac[0xf0].1 as u32); run -= 16; }
            let (size, b) = category(v);
            let sym = (run << 4 | size) as usize;
            bits.put(ac[sym].0 as u32, ac[sym].1 as u32);
            bits.put(b, size);
            run = 0;
        }
        if run > 0 { bits.put(ac[0].0 as u32, ac[0].1 as u32); } // end of block
    }

    /// Encode a `w` x `h` YUYV picture (BT.601 video range, as V4L2 and ffmpeg's yuyv422 give
    /// it); `w` a multiple of 16 and `h` of 8.
    pub fn encode_yuyv(&self, yuyv: &[u8], w: usize, h: usize) -> Vec<u8> {
        assert!(w % 16 == 0 && h % 8 == 0 && yuyv.len() >= w * h * 2);
        let mut out = Vec::with_capacity(w * h / 4);
        self.headers(w, h, &mut out);
        let mut bits = Bits { out, acc: 0, n: 0 };
        let mut pred = [0i32; 3];
        // video range to JPEG's full range, level-shifted
        let luma = |v: u8| (v as f32 - 16.0) * (255.0 / 219.0) - 128.0;
        let chroma = |v: u8| (v as f32 - 128.0) * (255.0 / 224.0);
        let (mut y0, mut y1, mut cb, mut cr) = ([0f32; 64], [0f32; 64], [0f32; 64], [0f32; 64]);
        for my in (0..h).step_by(8) {
            for mx in (0..w).step_by(16) {
                // one MCU: 16x8 pixels, two luma blocks and one block of each chroma
                for y in 0..8 {
                    let row = &yuyv[((my + y) * w + mx) * 2..((my + y) * w + mx + 16) * 2];
                    for p in 0..8 {
                        let q = &row[p * 4..p * 4 + 4];
                        let (ly, x) = if p < 4 { (&mut y0, p * 2) } else { (&mut y1, p * 2 - 8) };
                        ly[y * 8 + x] = luma(q[0]);
                        ly[y * 8 + x + 1] = luma(q[2]);
                        cb[y * 8 + p] = chroma(q[1]);
                        cr[y * 8 + p] = chroma(q[3]);
                    }
                }
                self.block(&y0, 0, &mut pred[0], &mut bits);
                self.block(&y1, 0, &mut pred[0], &mut bits);
                self.block(&cb, 1, &mut pred[1], &mut bits);
                self.block(&cr, 1, &mut pred[2], &mut bits);
            }
        }
        bits.flush();
        let mut out = bits.out;
        out.extend_from_slice(&[0xff, 0xd9]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn huffman_tables_are_the_standard_ones() {
        // T.81 Table K.3: DC luminance category 0 is 00, 5 is 110, 11 is 111111110
        let dc = Huff::new(&DC_LUMA_BITS, &DC_VALS);
        assert_eq!((dc.0[0], dc.0[5], dc.0[11]), ((0b00, 2), (0b110, 3), (0b1_1111_1110, 9)));
        // Table K.5: AC luminance EOB is 1010, ZRL is 11111111001
        let ac = Huff::new(&AC_LUMA_BITS, &AC_LUMA_VALS);
        assert_eq!((ac.0[0x00], ac.0[0xf0], ac.0[0x01]), ((0b1010, 4), (0b111_1111_1001, 11), (0b00, 2)));
    }

    #[test]
    fn categories() {
        assert_eq!(category(0), (0, 0));
        assert_eq!(category(1), (1, 1));
        assert_eq!(category(-1), (1, 0));
        assert_eq!(category(-3), (2, 0));
        assert_eq!(category(5), (3, 5));
    }

    /// A flat grey picture: well-formed markers, and each block is a DC difference and EOB.
    #[test]
    fn a_picture_is_a_complete_jfif_stream() {
        let w = 32;
        let h = 16;
        let yuyv = [128u8, 128].repeat(w * h);
        let j = Encoder::new(85).encode_yuyv(&yuyv, w, h);
        assert_eq!(&j[..4], &[0xff, 0xd8, 0xff, 0xe0]);
        assert_eq!(&j[j.len() - 2..], &[0xff, 0xd9]);
        let sof = j.windows(2).position(|m| m == [0xff, 0xc0]).unwrap();
        assert_eq!(&j[sof + 5..sof + 9], &[0, 16, 0, 32]);
        assert_eq!(j.windows(2).filter(|m| *m == [0xff, 0xc4]).count(), 4);
        // between SOS and EOI no marker but stuffed bytes
        let sos = j.windows(2).position(|m| m == [0xff, 0xda]).unwrap() + 14;
        let data = &j[sos..j.len() - 2];
        assert!(data.windows(2).all(|m| m[0] != 0xff || m[1] == 0));
        assert!(!data.is_empty() && data.len() < 64);
    }
}
