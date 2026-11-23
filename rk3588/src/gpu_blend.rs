//! Mali fixed-function blending for one linear RGBA8 UNORM render target.
//! Equations and constants follow Mesa v10.xml and pan_pack_blend_constant.
#[derive(Clone, Copy, Debug)]
struct Function(u16);
impl Function {
    fn decode(v: u32) -> Result<Self, ()> {
        if v & !0xfbb != 0 || v & 3 == 0 || (v >> 8) & 7 == 0 {
            return Err(());
        }
        Ok(Self(v as u16))
    }
    fn apply(self, channel: usize, s: [f32; 4], d: [f32; 4], constant: f32) -> f32 {
        let v = self.0;
        let mut a = match v & 3 {
            1 => 0.,
            2 => s[channel],
            3 => d[channel],
            _ => unreachable!(),
        };
        let mut b = match (v >> 4) & 3 {
            0 => s[channel] - d[channel],
            1 => s[channel] + d[channel],
            2 => s[channel],
            3 => d[channel],
            _ => unreachable!(),
        };
        let mut c = match (v >> 8) & 7 {
            1 => 0.,
            2 => s[channel],
            3 => d[channel],
            4 => 2. * s[channel],
            5 => s[3],
            6 => d[3],
            7 => constant,
            _ => unreachable!(),
        };
        if v & 8 != 0 {
            a = -a;
        }
        if v & 128 != 0 {
            b = -b;
        }
        if v & 2048 != 0 {
            c = 1. - c;
        }
        a + b * c
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Blend {
    rgb: Function,
    alpha: Function,
    mask: u32,
    constant: f32,
    /// The descriptor's 16-bit constant field, for [`Blend::for_bits`].
    raw_constant: u32,
    alpha_to_one: bool,
    rt: u8,
}
impl Blend {
    /// The constant as Mesa packs it for a render target whose first channel has `bits`
    /// bits (pack_blend_constant: unorm(c, bits) << (16 - bits)), so the field's maximum
    /// is (2^bits - 1) << (16 - bits): 0xff00 for 8 bits, 0xffc0 for 10, 0xf800 for 5.
    pub fn for_bits(self, bits: u32) -> Self {
        let bits = bits.clamp(1, 16);
        let max = ((1u32 << bits) - 1) << (16 - bits);
        Self {
            constant: (self.raw_constant as f32 / max as f32).min(1.),
            ..self
        }
    }
    pub fn decode(words: [u32; 4]) -> Result<Self, ()> {
        // A disabled Off descriptor has no equation or conversion to decode.
        if words[0] & 0x200 == 0 && words[2] & 3 == 3 {
            return Ok(Self {
                rgb: Function(0x122),
                alpha: Function(0x122),
                mask: 0,
                constant: 0.,
                raw_constant: 0,
                alpha_to_one: false,
                rt: ((words[2] >> 16) & 15) as u8,
            });
        }
        // Enabled, non-sRGB fixed-function/opaque output to any render target with a
        // non-raw conversion; no blend shader.
        if words[0] & 0xffff & !0x901 != 0x200
            || words[2] & !(3 | (3 << 3) | (15 << 16)) != 0
            || !matches!(words[2] & 3, 1 | 2)
            || words[3] & 0xffc0_0000 != 0
            || words[1] & 0x0f000000 != 0
        {
            return Err(());
        }
        let rgb = Function::decode(words[1] & 4095)?;
        let alpha = Function::decode((words[1] >> 12) & 4095)?;
        if words[2] & 3 == 1 && words[1] != 0xf0122122 {
            return Err(());
        }
        Ok(Self {
            rgb,
            alpha,
            mask: words[1] >> 28,
            constant: 0.,
            raw_constant: words[0] >> 16,
            alpha_to_one: words[0] & 256 != 0,
            rt: ((words[2] >> 16) & 15) as u8,
        }
        // an 8-bit target until the render target is known
        .for_bits(8))
    }
    /// Blend in floating point; `unorm` clamps sources, factors and results to [0, 1].
    pub fn apply_f(self, source: [f32; 4], dest: [f32; 4], unorm: bool) -> [f32; 4] {
        let mut source = if unorm {
            source.map(|v| v.clamp(0., 1.))
        } else {
            source
        };
        if self.alpha_to_one {
            source[3] = 1.;
        }
        // Replace (A = Src, B = Src, C = 0) with all channels written: the source.
        if self.mask == 15 && self.rgb.0 == 0x122 && self.alpha.0 == 0x122 {
            return source;
        }
        std::array::from_fn(|n| {
            if self.mask & (1 << n) == 0 {
                dest[n]
            } else {
                let f = if n == 3 { self.alpha } else { self.rgb };
                let v = f.apply(n, source, dest, self.constant);
                if unorm {
                    v.clamp(0., 1.)
                } else {
                    v
                }
            }
        })
    }
    pub fn rt(self) -> usize {
        self.rt as usize
    }
    pub fn apply(self, source: [f32; 4], destination: [u8; 4]) -> [u8; 4] {
        let mut source = source.map(|v| v.clamp(0., 1.));
        if self.alpha_to_one {
            source[3] = 1.;
        }
        let dest = destination.map(|v| v as f32 / 255.);
        std::array::from_fn(|n| {
            if self.mask & (1 << n) == 0 {
                destination[n]
            } else {
                let f = if n == 3 { self.alpha } else { self.rgb };
                (f.apply(n, source, dest, self.constant).clamp(0., 1.) * 255.).round() as u8
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn captured_source_over_preserves_alpha_and_draw_order() {
        let b = Blend::decode([0x201, 0xf0503503, 0x1a, 0xed000]).unwrap();
        let first = b.apply([1., 0., 0.5, 0.5], [0, 255, 64, 255]);
        assert_eq!(first, [128, 128, 96, 191]);
        let second = b.apply([0., 0., 1., 0.25], first);
        assert_eq!(second, [96, 96, 136, 159]);
        assert_ne!(
            b.apply(
                [1., 0., 0.5, 0.5],
                b.apply([0., 0., 1., 0.25], [0, 255, 64, 255])
            ),
            second
        );
        let separate = Blend::decode([0x201, 0xf0d32503, 0x1a, 0xed000]).unwrap();
        assert_eq!(
            separate.apply([1., 0., 0.5, 0.5], [0, 255, 64, 255]),
            [128, 128, 96, 255]
        );
    }
    #[test]
    fn constants_color_masks_and_alpha_to_one_obey_rgba8_conversion() {
        // A=Dest, B=Src-Dest, C=Constant, so K=0.5 is an exact midpoint.
        let b = Blend::decode([0x7f800201, 0x50703703, 0x1a, 0xed000]).unwrap();
        assert_eq!(
            b.apply([1., 0., 0., 0.5], [0, 255, 64, 128]),
            [128, 255, 32, 128]
        );
        let off = Blend::decode([0, 0, 3, 0]).unwrap();
        assert_eq!(off.apply([1.; 4], [12, 34, 56, 78]), [12, 34, 56, 78]);
        let opaque = Blend::decode([0x300, 0xf0122122, 0x19, 0xed000]).unwrap();
        assert_eq!(
            opaque.apply([2., -1., 0.5, 0.25], [99; 4]),
            [255, 0, 128, 255]
        );
    }
    #[test]
    fn operands_negation_and_inverse_follow_a_plus_b_times_c() {
        let s = [0.25, 0.5, 0.75, 0.5];
        let d = [0.5, 0.25, 1., 0.75];
        for (bits, expected) in [
            (0x122, 0.25),   // Src + Src*0
            (0x232, 0.375),  // Src + Dest*Src
            (0x2ba, -0.375), // -Src - Dest*Src
            (0xa32, 0.625),  // Src + Dest*(1-Src)
            (0x441, 0.375),  // reserved B bit: checked below rather than executed
        ] {
            if bits == 0x441 {
                assert!(Function::decode(bits).is_err());
                continue;
            }
            let f = Function::decode(bits).unwrap();
            assert_eq!(f.apply(0, s, d, 0.5), expected);
        }
        let twice = Function::decode(0x431).unwrap(); // 0 + Dest*(2*Src)
        assert_eq!(twice.apply(0, s, d, 0.), 0.25);
        assert_eq!(Function::decode(0x631).unwrap().apply(0, s, d, 0.), 0.375);
    }
    #[test]
    fn invalid_descriptor_fields_and_reserved_operands_are_rejected() {
        let good = [0x201, 0xf0503503, 0x1a, 0xed000];
        for (word, bit) in [(0, 1), (0, 10), (1, 2), (1, 6), (1, 24), (2, 5), (3, 22)] {
            let mut w = good;
            w[word] |= 1 << bit;
            assert!(Blend::decode(w).is_err(), "word={word} bit={bit}");
        }
        for mode in [0, 3] {
            let mut w = good;
            w[2] = (w[2] & !3) | mode;
            assert!(Blend::decode(w).is_err());
        }
        for bad in [0, 0x120, 0x40, 0x800] {
            assert!(Function::decode(bad).is_err());
        }
        // Constants above 0xff00 are what Mesa packs for 10-bit targets: 1.0 is 0xffc0.
        let mut w = good;
        w[0] |= 0xffc0_0000;
        let b = Blend::decode(w).unwrap();
        assert_eq!(b.for_bits(10).constant, 1.);
        assert_eq!(b.for_bits(8).constant, 1.);
    }
    #[test]
    fn the_constant_follows_the_target_precision() {
        // pack_blend_constant(1.0) for 4, 5, 8 and 10 bits
        for (raw, bits) in [(0xf000u32, 4), (0xf800, 5), (0xff00, 8), (0xffc0, 10)] {
            let b = Blend::decode([raw << 16 | 0x201, 0x50703703, 0x1a, 0xed000]).unwrap();
            assert_eq!(b.for_bits(bits).constant, 1., "{bits} bits");
        }
        // 0.5 on a 5-bit target: unorm 15 -> 15/31
        let b = Blend::decode([0x7800_0201, 0x50703703, 0x1a, 0xed000]).unwrap();
        assert_eq!(b.for_bits(5).constant, 15. / 31.);
    }
}
