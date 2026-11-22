//! The shader program header: the 80 bytes before the code of a graphics shader.

/// Size of the header in bytes; the code follows it.
pub const SIZE: u32 = 0x50;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ShaderType {
    Vertex,
    TessControl,
    TessEval,
    Geometry,
    Fragment,
}

/// Primitive a geometry shader makes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Topology {
    Points,
    LineStrip,
    TriangleStrip,
}

/// How a fragment shader input is interpolated.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Interp {
    Unused,
    /// Flat.
    Constant,
    Perspective,
    ScreenLinear,
}

/// Where a fragment shader leaves its results: numbers of general registers.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct FragOutputs {
    /// Register of each component of each render target.
    pub color: [[Option<u8>; 4]; 8],
    pub sample_mask: Option<u8>,
    pub depth: Option<u8>,
}

/// A decoded program header.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sph {
    /// The header's words.
    pub words: [u32; 20],
    pub ty: ShaderType,
    /// Colour output 0 goes to every render target (gl_FragColor).
    pub color0_broadcast: bool,
    pub kills_pixels: bool,
    pub global_store: bool,
    pub load_or_store: bool,
    pub fp64: bool,
    /// Vertex streams of a geometry shader (a mask).
    pub streams: u8,
    /// Local memory per lane: the positive and the negative part, and the call stack's share.
    pub local_low: u32,
    pub local_high: u32,
    pub local_crs: u32,
    /// Output components per patch (tessellation control).
    pub patch_attributes: u32,
    /// Invocations per primitive (tessellation control: output vertices; geometry: instances).
    pub threads_per_primitive: u32,
    pub topology: Option<Topology>,
    pub max_vertices: u32,
}

impl Sph {
    /// None if this is not a program header.
    pub fn parse(bytes: &[u8]) -> Option<Sph> {
        let mut words = [0; 20];
        for (n, w) in words.iter_mut().enumerate() {
            *w = u32::from_le_bytes(bytes.get(4 * n..4 * n + 4)?.try_into().unwrap());
        }
        let w = &words;
        let ty = match (w[0] >> 10 & 15, w[0] & 31) {
            (1, 1) => ShaderType::Vertex,
            (2, 1) => ShaderType::TessControl,
            (3, 1) => ShaderType::TessEval,
            (4, 1) => ShaderType::Geometry,
            (5, 2) => ShaderType::Fragment,
            _ => return None,
        };
        let topology = match w[3] >> 24 & 15 {
            1 => Some(Topology::Points),
            6 => Some(Topology::LineStrip),
            7 => Some(Topology::TriangleStrip),
            _ => None,
        };
        // On Maxwell the number of patch attributes moved: 4 bits in word 3, 4 in word 4.
        let moved = w[3] >> 28 | (w[4] >> 20 & 15) << 4;
        let old = w[1] >> 24;
        Some(Sph {
            words,
            ty,
            // the bit says that targets have their own outputs
            color0_broadcast: w[0] >> 14 & 1 == 0,
            kills_pixels: w[0] >> 15 & 1 != 0,
            global_store: w[0] >> 16 & 1 != 0,
            load_or_store: w[0] >> 26 & 1 != 0,
            fp64: w[0] >> 27 & 1 != 0,
            streams: (w[0] >> 28) as u8,
            local_low: w[1] & 0xff_ffff,
            local_high: w[2] & 0xff_ffff,
            local_crs: w[3] & 0xff_ffff,
            patch_attributes: if moved != 0 { moved } else { old },
            threads_per_primitive: w[2] >> 24,
            topology,
            max_vertices: w[4] & 0xfff,
        })
    }

    fn bits(&self, at: u32, n: u32) -> u32 {
        self.words[at as usize / 32] >> (at % 32) & ((1 << n) - 1)
    }

    /// Whether a vertex, tessellation or geometry shader reads the input attribute at this byte
    /// address. For a fragment shader: whether the input is interpolated at all.
    pub fn reads(&self, addr: u32) -> bool {
        if self.ty == ShaderType::Fragment {
            return self.interp(addr) != Interp::Unused;
        }
        addr < 0x380 && self.bits(160 + addr / 4, 1) != 0
    }

    /// Whether a vertex, tessellation or geometry shader writes the output attribute at this
    /// byte address.
    pub fn writes(&self, addr: u32) -> bool {
        self.ty != ShaderType::Fragment
            && (0x40..0x380).contains(&addr)
            && self.bits(400 + addr / 4, 1) != 0
    }

    /// How a fragment shader's input at this byte address is interpolated. System values
    /// (position, layer, ...) report `ScreenLinear` or `Constant` when used.
    pub fn interp(&self, addr: u32) -> Interp {
        let two = |at: u32| {
            [Interp::Unused, Interp::Constant, Interp::Perspective, Interp::ScreenLinear]
                [self.bits(at, 2) as usize]
        };
        let one = |at: u32, i: Interp| if self.bits(at, 1) != 0 { i } else { Interp::Unused };
        let c = addr / 4;
        match addr {
            // primitive id, layer, viewport index
            0x60..=0x6b => one(160 + c, Interp::Constant),
            // point size, position
            0x6c..=0x7f => one(160 + c, Interp::ScreenLinear),
            0x80..=0x27f => two(192 + 2 * (c - 0x20)),
            0x280..=0x29f => two(448 + 2 * (c - 0xa0)),
            // clip distances
            0x2c0..=0x2df => one(464 + (c - 0xb0), Interp::Perspective),
            // point sprite coordinates
            0x2e0..=0x2e7 => one(472 + (c - 0xb8), Interp::ScreenLinear),
            // fog
            0x2e8 => one(474, Interp::Perspective),
            // tessellation coordinates, instance and vertex numbers
            0x2f0..=0x2ff => one(476 + (c - 0xbc), Interp::Constant),
            0x300..=0x39f => two(480 + 2 * (c - 0xc0)),
            _ => Interp::Unused,
        }
    }

    /// The input attributes used, as byte addresses in order.
    pub fn inputs(&self) -> impl Iterator<Item = u32> + '_ {
        (0..0x3a0).step_by(4).filter(|&a| self.reads(a))
    }

    /// The output attributes of a vertex, tessellation or geometry shader.
    pub fn outputs(&self) -> impl Iterator<Item = u32> + '_ {
        (0x40..0x380).step_by(4).filter(|&a| self.writes(a))
    }

    /// The registers with a fragment shader's results: the components of the render targets one
    /// after the other, the sample mask in the next register and the depth in the one after
    /// that (whether there is a sample mask or not).
    pub fn frag_outputs(&self) -> FragOutputs {
        let mut o = FragOutputs::default();
        if self.ty != ShaderType::Fragment {
            return o;
        }
        let mut r = 0;
        for rt in 0..8 {
            for c in 0..4 {
                if self.bits(576 + 4 * rt + c, 1) != 0 {
                    o.color[rt as usize][c as usize] = Some(r);
                    r += 1;
                }
            }
        }
        if self.bits(608, 1) != 0 {
            o.sample_mask = Some(r);
        }
        if self.bits(609, 1) != 0 {
            o.depth = Some(r + 1);
        }
        o
    }

    /// The range of vertex attributes a tessellation shader reads back from its outputs (byte
    /// addresses of the first and last), if any.
    pub fn output_reads(&self) -> Option<(u32, u32)> {
        let (min, max) = (self.words[4] >> 12 & 0xff, self.words[4] >> 24);
        (min <= max).then_some((min * 4, max * 4))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(w: [u32; 20]) -> Vec<u8> {
        w.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    /// What nvc0_fp_gen_header makes for two colour targets, depth, a perspective generic, a
    /// flat generic, a colour and gl_FragCoord.xy.
    #[test]
    fn fragment() {
        let mut w = [0; 20];
        w[0] = 0x20062 | 5 << 10 | 0x8000;
        w[1] = 0x20;
        w[5] = 0x8000_0000 | 3 << 28;
        // generic 0 xyzw perspective
        let set = |w: &mut [u32; 20], addr: u32, m: u32| {
            let a = addr / 4 * 2 - if addr >= 0x300 { 32 } else { 0 };
            w[4 + a as usize / 32] |= m << (a % 32);
        };
        for c in 0..4 {
            set(&mut w, 0x80 + 4 * c, 2);
        }
        set(&mut w, 0x94, 1);
        set(&mut w, 0x280, 3);
        set(&mut w, 0x310, 2);
        w[18] = 0xf0f;
        w[19] = 2;
        w[0] |= 1 << 14; // separate outputs
        let s = Sph::parse(&bytes(w)).unwrap();
        assert_eq!(s.ty, ShaderType::Fragment);
        assert!(s.kills_pixels && !s.global_store && !s.color0_broadcast);
        assert_eq!(s.local_low, 0x20);
        assert_eq!(s.interp(0x80), Interp::Perspective);
        assert_eq!(s.interp(0x8c), Interp::Perspective);
        assert_eq!(s.interp(0x90), Interp::Unused);
        assert_eq!(s.interp(0x94), Interp::Constant);
        assert_eq!(s.interp(0x280), Interp::ScreenLinear);
        assert_eq!(s.interp(0x310), Interp::Perspective);
        assert_eq!(s.interp(0x70), Interp::ScreenLinear);
        assert_eq!(s.interp(0x78), Interp::Unused);
        let i: Vec<u32> = s.inputs().collect();
        assert_eq!(i, [0x70, 0x74, 0x7c, 0x80, 0x84, 0x88, 0x8c, 0x94, 0x280, 0x310]);
        let o = s.frag_outputs();
        assert_eq!(o.color[0], [Some(0), Some(1), Some(2), Some(3)]);
        assert_eq!(o.color[1], [None; 4]);
        assert_eq!(o.color[2], [Some(4), Some(5), Some(6), Some(7)]);
        assert_eq!((o.sample_mask, o.depth), (None, Some(9)));
    }

    /// nvc0_vp_gen_header / nvc0_tcp_gen_header / nvc0_gp_gen_header.
    #[test]
    fn vertex_tess_geometry() {
        let mut w = [0; 20];
        w[0] = 0x20061 | 1 << 10;
        w[4] = 0xff000;
        w[5 + 0x20 / 32] |= 0xf;
        w[10] |= 1 << 31;
        w[13] |= 0xf << (0x1c - 0x10);
        let s = Sph::parse(&bytes(w)).unwrap();
        assert_eq!(s.ty, ShaderType::Vertex);
        assert_eq!(s.inputs().collect::<Vec<_>>(), [0x80, 0x84, 0x88, 0x8c, 0x2fc]);
        assert_eq!(s.outputs().collect::<Vec<_>>(), [0x70, 0x74, 0x78, 0x7c]);
        assert_eq!(s.output_reads(), None);

        let mut w = [0; 20];
        let opcs = 0x2c;
        w[0] = 0x20061 | 2 << 10;
        w[1] = opcs << 24;
        w[2] = 4 << 24;
        w[3] = (opcs & 0xf) << 28;
        w[4] = 0x20 << 24 | 0x1c << 12 | (opcs & 0xf0) << 16;
        let s = Sph::parse(&bytes(w)).unwrap();
        assert_eq!((s.ty, s.patch_attributes, s.threads_per_primitive), (ShaderType::TessControl, 0x2c, 4));
        assert_eq!(s.output_reads(), Some((0x70, 0x80)));

        let mut w = [0; 20];
        w[0] = 0x20061 | 4 << 10 | 0x1000_0000;
        w[2] = 1 << 24;
        w[3] = 0x0700_0000;
        w[4] = 12;
        let s = Sph::parse(&bytes(w)).unwrap();
        assert_eq!((s.topology, s.max_vertices, s.streams), (Some(Topology::TriangleStrip), 12, 1));
    }
}
