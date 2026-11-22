//! Runs real nouveau code (compiled by Mesa's nv50_ir for GM20B) and compares with the results
//! of Mesa's softpipe. The corpus is made outside the repository: MAXWELL_CORPUS names a
//! directory with one directory per shader, each with the code segment (`NNN-FS.bin`,
//! `NNN-CS.bin`: the built-in library at 0, the program header at 0x7b0, code at 0x800) and
//! the reference (`ref`); MAXWELL_UBO names the uniform buffer.

use maxwell_shader::isa::*;
use maxwell_shader::*;

const W: usize = 8;

struct Gpu {
    ubo: Vec<u32>,
    tex_cb: Vec<u32>,
    x: usize,
    y: usize,
    mem: Vec<u8>,
    shared: Vec<u8>,
    group: u32,
    tid: Vec<u32>,
    /// Outputs of the vertex shader by vertex.
    vtx: Vec<Vec<u32>>,
    sph: Option<Sph>,
}

const SSBO: u64 = 0x1_0000_0000;

impl<const N: usize> Host<N> for Gpu {
    fn cbuf(&self, index: u32, offset: u32) -> u32 {
        let o = offset as usize / 4;
        match index {
            // the state variable for gl_FragCoord: the shader flips, so does softpipe
            0 => [1f32, 0.0, -1.0, 8.0].get(o).map_or(0, |x| x.to_bits()),
            1 => self.ubo.get(o).copied().unwrap_or(0),
            // compute: buffer 0 in the auxiliary buffer
            7 | 15 => match offset {
                0x2a0 => SSBO as u32,
                0x2a4 => (SSBO >> 32) as u32,
                0x2a8 => 4096,
                0x100 => 1,
                _ => self.tex_cb.get(o).copied().unwrap_or(0),
            },
            _ => 0,
        }
    }
    fn sysval(&mut self, sys: u8) -> [u32; N] {
        match sys {
            0x20 => std::array::from_fn(|i| self.tid[i]),
            _ => [0; N],
        }
    }
    fn load(&mut self, addr: u64, out: &mut [u8]) {
        out.fill(0);
        if let Some(o) = addr.checked_sub(SSBO) {
            if let Some(s) = self.mem.get(o as usize..o as usize + out.len()) {
                out.copy_from_slice(s);
            }
        }
    }
    fn store(&mut self, addr: u64, data: &[u8]) {
        if let Some(o) = addr.checked_sub(SSBO) {
            if let Some(s) = self.mem.get_mut(o as usize..o as usize + data.len()) {
                s.copy_from_slice(data);
            }
        }
    }
    fn shared(&mut self) -> &mut [u8] {
        &mut self.shared
    }
    fn attr_load(&mut self, lane: usize, a: Attr) -> u32 {
        if a.addr == 0x2fc {
            lane as u32
        } else {
            0
        }
    }
    fn attr_store(&mut self, lane: usize, a: Attr, v: u32) {
        self.vtx[lane][a.addr as usize / 4] = v;
    }
    fn interpolate(&mut self, op: &IpaOp<N>) -> [u32; N] {
        std::array::from_fn(|i| {
            let x = (self.x + i % 2) as f32 + 0.5 + op.offset[0][i];
            let y = (self.y + i / 2) as f32 + 0.5 + op.offset[1][i];
            match op.addr {
                0x70 => x.to_bits(),
                0x74 => (W as f32 - y).to_bits(),
                0x78 => 0.5f32.to_bits(),
                0x7c => 1f32.to_bits(),
                0x3fc => !0,
                a => {
                    // A triangle from (-1, -1) to (3, -1) and (-1, 3), w = 1.
                    let v: Vec<u32> = (0..3).map(|k| self.vtx[k][a as usize / 4]).collect();
                    if self.sph.unwrap().interp(a) == sph::Interp::Constant {
                        return v[2];
                    }
                    let v: Vec<f32> = v.into_iter().map(f32::from_bits).collect();
                    let (b1, b2) = (x / W as f32 / 2.0, y / W as f32 / 2.0);
                    (v[0] * (1.0 - b1 - b2) + v[1] * b1 + v[2] * b2).to_bits()
                }
            }
        })
    }
    fn texture(&mut self, t: &TexOp<N>) -> [[u32; N]; 4] {
        common::texture(t)
    }
}

mod common;

fn words(b: &[u8]) -> Vec<u32> {
    b.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect()
}

fn fetch(code: &[u8]) -> impl FnMut(u32) -> Option<u64> + '_ {
    |a| {
        let a = a as usize;
        Some(u64::from_le_bytes(code.get(a..a + 8)?.try_into().unwrap()))
    }
}

fn close(a: u32, b: u32) -> bool {
    if a == b {
        return true;
    }
    let (x, y) = (f32::from_bits(a), f32::from_bits(b));
    if x.is_nan() && y.is_nan() {
        return true;
    }
    (x - y).abs() <= (x.abs().max(y.abs()) * 2e-5).max(1e-6)
}

#[test]
#[ignore = "needs a corpus"]
fn corpus() {
    let dir = std::env::var("MAXWELL_CORPUS").unwrap();
    let ubo = words(&std::fs::read(std::env::var("MAXWELL_UBO").unwrap()).unwrap());
    let only = std::env::var("MAXWELL_ONLY").ok();
    let (mut ran, mut bad, mut decoded) = (0, 0, 0);
    let mut dirs: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    dirs.sort();
    for d in dirs {
        let name = d.file_name().unwrap().to_string_lossy().to_string();
        if only.as_ref().is_some_and(|o| *o != name) {
            continue;
        }
        let mut reference = std::fs::read(d.join("ref")).ok().map(|b| words(&b));
        // Code compiled with the other compiler options has its reference elsewhere.
        if reference.is_none() {
            if let Ok(r) = std::env::var("MAXWELL_REF") {
                reference = std::fs::read(format!("{r}/{name}/ref")).ok().map(|b| words(&b));
            }
        }
        for f in std::fs::read_dir(&d).unwrap() {
            let f = f.unwrap().path();
            let n = f.file_name().unwrap().to_string_lossy().to_string();
            if !n.ends_with(".bin") {
                continue;
            }
            let code = std::fs::read(&f).unwrap();
            let p = match Program::decode_at(0x800, &mut fetch(&code)) {
                Ok(p) => p,
                Err(e) => {
                    println!("{name}/{n}: {e}");
                    bad += 1;
                    continue;
                }
            };
            decoded += 1;
            if p.any(|o| matches!(o, Op::Invalid(_))) {
                println!("{name}/{n}: unknown instruction\n{p}");
                bad += 1;
                continue;
            }
            if only.is_some() {
                println!("{name}/{n}\n{p}");
            }
            let mut gpu = Gpu {
                ubo: ubo.clone(),
                tex_cb: (0..0x400).collect(),
                x: 0,
                y: 0,
                mem: vec![0; 4096],
                shared: vec![0; 4096],
                group: 0,
                tid: vec![],
                vtx: vec![vec![0; 0x100]; 4],
                sph: None,
            };
            let Some(reference) = &reference else { continue };
            let mut got = vec![0xdead_beefu32; reference.len()];
            if n.contains("-FS") {
                let sph = Sph::parse(&code[0x7b0..]).unwrap();
                gpu.sph = Some(sph);
                let vs = std::fs::read_dir(&d)
                    .unwrap()
                    .map(|f| f.unwrap().path())
                    .find(|f| f.to_string_lossy().ends_with("-VS.bin"))
                    .unwrap();
                let vs = std::fs::read(vs).unwrap();
                let vp = Program::decode_at(0x800, &mut fetch(&vs)).unwrap();
                let mut l = Lanes::<4>::new();
                l.start(&vp, 7, 0);
                assert_eq!(run(&vp, &mut l, &mut gpu, &mut 100_000), Exit::Done);
                let out = sph.frag_outputs();
                let mut l = Lanes::<4>::new();
                l.set_local_size(sph.local_low as usize);
                for qy in (0..W).step_by(2) {
                    for qx in (0..W).step_by(2) {
                        (gpu.x, gpu.y) = (qx, qy);
                        l.start(&p, 15, 0);
                        let mut fuel = 1_000_000;
                        let e = run(&p, &mut l, &mut gpu, &mut fuel);
                        if e != Exit::Done {
                            println!("{name}/{n}: {e:?}");
                        }
                        for i in 0..4 {
                            if l.killed() >> i & 1 != 0 {
                                continue;
                            }
                            let at = ((qy + i / 2) * W + qx + i % 2) * 4;
                            for c in 0..4 {
                                got[at + c] = l.gpr[out.color[0][c].unwrap() as usize][i];
                            }
                        }
                    }
                }
            } else if n.contains("-CS") {
                gpu.group = 1;
                // local size 8 x 2 x 1, two groups of 8 lanes
                let mut gs: Vec<_> = (0..2).map(|_| Lanes::<8>::new()).collect();
                for l in &mut gs {
                    l.set_local_size(1024);
                    l.start(&p, 0xff, 0);
                }
                let mut going = [true; 2];
                while going.contains(&true) {
                    for (k, l) in gs.iter_mut().enumerate() {
                        if !going[k] {
                            continue;
                        }
                        gpu.tid = (0..8).map(|i| i | (k as u32) << 16).collect();
                        let mut fuel = 1_000_000;
                        match run(&p, l, &mut gpu, &mut fuel) {
                            Exit::Barrier => {}
                            Exit::Done => going[k] = false,
                            e => {
                                println!("{name}/{n}: {e:?}");
                                going[k] = false;
                            }
                        }
                    }
                }
                got = words(&gpu.mem);
            } else {
                continue;
            }
            ran += 1;
            let errs: Vec<usize> = (0..got.len()).filter(|&i| !close(got[i], reference[i])).collect();
            if !errs.is_empty() {
                bad += 1;
                println!("{name}/{n}: {} of {} words differ", errs.len(), got.len());
                for &i in errs.iter().take(6) {
                    println!(
                        "  [{}].{} got {:08x} {:e}  want {:08x} {:e}",
                        i / 4,
                        i % 4,
                        got[i],
                        f32::from_bits(got[i]),
                        reference[i],
                        f32::from_bits(reference[i])
                    );
                }
            }
        }
    }
    println!("{decoded} decoded, {ran} run, {bad} bad");
    assert_eq!(bad, 0);
}
