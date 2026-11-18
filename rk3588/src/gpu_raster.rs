//! Single-sample triangle rasterization and native opaque fragment shaders.
use crate::{
    gpu_cs::Stop,
    gpu_mmu::AddressSpace,
    gpu_resources::Resources,
    gpu_shader::{Fragment, RegisterAllocation, Shader},
    gpu_tiler::Frame,
};
fn word(b: &[u8], n: usize) -> u32 {
    u32::from_le_bytes(b[n * 4..n * 4 + 4].try_into().unwrap())
}
fn pair(b: &[u8], n: usize) -> u64 {
    word(b, n) as u64 | ((word(b, n + 1) as u64) << 32)
}
fn reg(r: &[u32; 256], n: usize) -> u64 {
    r[n] as u64 | ((r[n + 1] as u64) << 32)
}
fn bad() -> Stop {
    Stop::Unsupported { pc: 0, opcode: 7 }
}
fn edge(a: [f32; 4], b: [f32; 4], x: f32, y: f32) -> f32 {
    (b[0] - a[0]) * (y - a[1]) - (b[1] - a[1]) * (x - a[0])
}
fn inclusive(a: [f32; 4], b: [f32; 4]) -> bool {
    b[1] < a[1] || b[1] == a[1] && b[0] < a[0]
}
pub fn pixels(
    m: &AddressSpace,
    ram: &mut [u8],
    frame: &Frame,
    bounds: [u32; 4],
    frame_arg: u64,
) -> Result<Vec<(u32, u32, [u8; 4])>, Stop> {
    let [x0, y0, x1, y1] = bounds;
    let mut output = Vec::new();
    let mut remaining: usize = 1 << 22;
    for draw in &frame.draws {
        let r = &draw.regs;
        // No depth/stencil, culling, multisampling, occlusion or alpha-to-coverage yet.
        if r[57] & ((1 << 11) | (3 << 14) | (3 << 17) | (15 << 19)) != 0
            || r[58] & !0x0001_ffff != 0
            || r[58] >> 16 != 1
            || reg(r, 46) != 0
        {
            return Err(bad());
        }
        let mut blend = [0; 16];
        if reg(r, 50) & 63 != 1 {
            return Err(bad());
        }
        m.read(ram, reg(r, 50) & !63, &mut blend, true)
            .map_err(Stop::Memory)?;
        if std::env::var_os("SBC_MALI_TRACE").is_some() {
            eprintln!(
                "[mali] blend={:x?} DCD={:x?}",
                blend
                    .chunks_exact(4)
                    .map(|v| u32::from_le_bytes(v.try_into().unwrap()))
                    .collect::<Vec<_>>(),
                &r[57..59]
            );
        }
        if word(&blend, 0) != 0x200
            || word(&blend, 1) != 0xf012_2122
            || word(&blend, 2) != 0x19
            || word(&blend, 3) != 237 << 12
        {
            return Err(bad());
        }
        let mut zs = [0; 32];
        m.read(ram, reg(r, 52), &mut zs, true)
            .map_err(Stop::Memory)?;
        if word(&zs, 0) & 15 != 7
            || word(&zs, 0) & (3 << 30) != 0
            || word(&zs, 4) & (3 << 27) != 0
            || word(&zs, 4) >> 29 != 7
        {
            return Err(bad());
        }
        let mut spd = [0; 32];
        m.read(ram, reg(r, 20), &mut spd, true)
            .map_err(Stop::Memory)?;
        if word(&spd, 0) & !((3 << 30) | (1 << 28)) != 0x128
            || word(&spd, 1) & !0xf800 != 0
            || spd[16..].iter().any(|&v| v != 0)
        {
            return Err(bad());
        }
        let allocation = match word(&spd, 0) >> 30 {
            0 => RegisterAllocation::Registers64,
            2 => RegisterAllocation::Registers32,
            _ => return Err(bad()),
        };
        let fau = reg(r, 12);
        let count = (fau >> 56) as usize;
        if count > 128 || fau & 0x00ff_0000_0000_0000 != 0 || pair(&spd, 2) & 7 != 0 {
            return Err(bad());
        }
        let mut data = vec![0; count * 8];
        m.read(ram, fau & 0xffff_ffff_ffff, &mut data, true)
            .map_err(Stop::Memory)?;
        let uniforms: Vec<u32> = data
            .chunks_exact(4)
            .map(|v| u32::from_le_bytes(v.try_into().unwrap()))
            .collect();
        if r[58] & 1 == 0 {
            continue;
        }
        for triangle in &draw.triangles {
            let mut v = *triangle;
            let area = edge(v[0], v[1], v[2][0], v[2][1]);
            if area == 0.0 {
                continue;
            }
            if area < 0.0 {
                v.swap(1, 2);
            }
            let minx = v
                .iter()
                .map(|p| p[0])
                .fold(f32::INFINITY, f32::min)
                .floor()
                .max(x0 as f32)
                .max((r[42] & 65535) as f32) as u32;
            let miny = v
                .iter()
                .map(|p| p[1])
                .fold(f32::INFINITY, f32::min)
                .floor()
                .max(y0 as f32)
                .max((r[42] >> 16) as f32) as u32;
            let maxx = v
                .iter()
                .map(|p| p[0])
                .fold(f32::NEG_INFINITY, f32::max)
                .ceil()
                .min(x1 as f32)
                .min((r[43] & 65535) as f32)
                .max(0.0) as u32;
            let maxy = v
                .iter()
                .map(|p| p[1])
                .fold(f32::NEG_INFINITY, f32::max)
                .ceil()
                .min(y1 as f32)
                .min((r[43] >> 16) as f32)
                .max(0.0) as u32;
            for y in miny..=maxy {
                for x in minx..=maxx {
                    let inside = (0..3).all(|n| {
                        let a = v[n];
                        let b = v[(n + 1) % 3];
                        let e = edge(a, b, x as f32 + 0.5, y as f32 + 0.5);
                        e > 0.0 || e == 0.0 && inclusive(a, b)
                    });
                    if !inside {
                        continue;
                    }
                    let mut shader = Shader::with_allocation(pair(&spd, 2), allocation);
                    shader.resources = Resources(reg(
                        r,
                        if draw.instruction & (1 << 38) != 0 {
                            4
                        } else {
                            0
                        },
                    ));
                    shader.fragment = Some(Fragment {
                        blend: [word(&blend, 2), word(&blend, 3)],
                        color: None,
                        active: true,
                    });
                    let values = [
                        x | (y << 16),
                        1,
                        1,
                        frame_arg as u32,
                        (frame_arg >> 32) as u32,
                    ];
                    for (n, &value) in values.iter().enumerate() {
                        if word(&spd, 1) & (1 << (11 + n)) != 0 {
                            shader.regs[59 + n] = value;
                        }
                    }
                    shader
                        .run(m, ram, &uniforms, remaining.min(4096))
                        .map_err(Stop::Shader)?;
                    remaining -= shader.instructions as usize;
                    if let Some(color) = shader.fragment.unwrap().color {
                        if output.len() >= 1 << 20 {
                            return Err(bad());
                        }
                        output.push((x, y, color.map(|v| (v.clamp(0., 1.) * 255.).round() as u8)));
                    }
                }
            }
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn covered(mut v: [[f32; 4]; 3], x: f32, y: f32) -> bool {
        if edge(v[0], v[1], v[2][0], v[2][1]) < 0. {
            v.swap(1, 2);
        }
        (0..3).all(|n| {
            let a = v[n];
            let b = v[(n + 1) % 3];
            let e = edge(a, b, x, y);
            e > 0. || e == 0. && inclusive(a, b)
        })
    }
    #[test]
    fn shared_edge_has_one_owner_for_both_windings() {
        let a = [0., 0., 0., 1.];
        let b = [2., 0., 0., 1.];
        let c = [2., 2., 0., 1.];
        let d = [0., 2., 0., 1.];
        for first in [[a, b, c], [c, b, a]] {
            for second in [[a, c, d], [d, c, a]] {
                for y in 0..2 {
                    for x in 0..2 {
                        let count = covered(first, x as f32 + 0.5, y as f32 + 0.5) as u32
                            + covered(second, x as f32 + 0.5, y as f32 + 0.5) as u32;
                        assert_eq!(count, 1);
                    }
                }
            }
        }
    }
}
