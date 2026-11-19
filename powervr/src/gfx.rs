//! The 3D pipeline: geometry (VDM, vertex shading, the TA/PPP), fragment (tiles, the ISP,
//! pixel shading, the PBE) and the transfer queue's 3D path, played at the level of the
//! firmware commands (`rogue_fwif_cmd_geom`, `_frag`, `_transfer`).
//!
//! There is no parameter buffer: a geometry job appends its primitives, with a snapshot of
//! the PPP state they were drawn with, to a scene keyed by the HWRT data the job names; the
//! fragment job with the same HWRT data renders that scene and drops it. A FRAG_PR (the
//! partial render the firmware would run when the parameter buffer fills up) never has
//! anything to do. Everything programmable runs Mesa's own programs on the PDS and USC
//! interpreters:
//!
//! - geometry: the VDM control stream's PDS state program fills the vertex shaders' shared
//!   registers; per vertex, the vertex PDS program (temps 0/1 = index / instance) DMAs the
//!   attributes into the unified store and starts the vertex shader, whose `uvsw.write`s
//!   are the vertex outputs (position in 0..3);
//! - fragment, per 16x16 tile: the background object (texture-state PDS program into the
//!   shared registers, then its kick program and USC "load op" shader on every pixel), then
//!   each primitive covering pixels: the coefficient PDS program (DOUTI: the TSP's plane
//!   set-ups, computed here), the uniform PDS program, and the fragment shader on every
//!   covered pixel; then the end-of-tile PDS program and USC program, whose `emitpix`
//!   makes the PBE write the tile's output registers to memory.
//!
//! Varyings are planes A*x + B*y + C in pixel-centre coordinates relative to the tile; the
//! USC's iterators (`ditr`, `fitr`) evaluate them with the same convention (the hardware's
//! origin is not documented; only the agreement matters).
use crate::cdm::{GpuMem, TaskHost, PDS_HEAP, USC_HEAP};
use crate::pds::{self, Pds};
use crate::tpu::twiddle;
use crate::usc::{self, Instance};
use std::collections::HashMap;

const TILE: u32 = 16;

/// A vertex after the vertex shader and the viewport transform.
#[derive(Clone, Debug, Default)]
struct Vertex {
    /// window x, y, z and 1/w
    win: [f32; 4],
    /// the vertex shader's outputs
    out: Vec<u32>,
}

/// The PPP (primitive processing) state words a primitive was submitted with.
#[derive(Clone, Debug, Default)]
struct Ppp {
    ispctl: u32,
    ispa: u32,
    ispb: u32,
    ispa_back: u32,
    ispb_back: u32,
    dbsc: u32,
    /// SHADERBASE, TEXUNICODEBASE, SIZEINFO1, SIZEINFO2
    pds: [u32; 4],
    varyingbase: u32,
    texturedatabase: u32,
    uniformdatabase: u32,
    region_clip: [u32; 2],
    /// a0, m0, a1, m1, a2, m2 per viewport
    viewports: Vec<[f32; 6]>,
    wclamp: f32,
    outsel: u32,
    varying: [u32; 3],
    ppp_ctrl: u32,
}

#[derive(Clone, Debug)]
struct Prim {
    v: [Vertex; 3],
    ppp: Ppp,
    /// the primitive faces away (clockwise in window coordinates)
    back: bool,
}

/// The primitives of one scene (render).
#[derive(Default)]
pub struct Scene {
    prims: Vec<Prim>,
}

/// State kept between kicks: scenes waiting for their fragment job.
#[derive(Default)]
pub struct Gfx {
    scenes: HashMap<u32, Scene>,
    pub draws: u64,
    pub prims: u64,
    pub tiles: u64,
}

/// One job's working state.
struct Job<'a, 'b> {
    mem: &'a mut GpuMem<'b>,
    pds: Pds,
    cache: HashMap<u64, Result<usc::Group, usc::DecodeError>>,
    trace: bool,
}

fn u64_at(w: &[u32], byte: usize) -> u64 {
    w[byte / 4] as u64 | (w[byte / 4 + 1] as u64) << 32
}

fn f(v: u32) -> f32 {
    f32::from_bits(v)
}

impl Job<'_, '_> {
    fn read(&mut self, va: u64) -> Result<u32, String> {
        self.mem.read32(va).ok_or(format!("read from unmapped {va:#x}"))
    }

    fn words(&mut self, va: u64, n: usize) -> Result<Vec<u32>, String> {
        (0..n).map(|i| self.read(va + 4 * i as u64)).collect()
    }

    /// Run a PDS program (code and data as PDS-heap offsets): writes to the common store go
    /// to `common`, to the unified store to `unified`; returns its DOUTU and DOUTI issues.
    #[allow(clippy::too_many_arguments)]
    fn pds(&mut self, code: u64, data: u64, data_words: usize, temps: Vec<u32>, if1: bool, common: &mut [u32], unified: &mut [u32]) -> Result<(Option<pds::DoutU>, Vec<u64>), String> {
        let data = self.words(PDS_HEAP + data, data_words)?;
        if self.trace {
            let code_words: Vec<u32> = (0..64).map_while(|i| self.mem.read32(PDS_HEAP + code + 4 * i)).collect();
            eprintln!("[gpu pds] program {code:#x} data {:x?}", data);
            for l in pds::disassemble(&code_words[..pds::program_len(&code_words)]).lines() {
                eprintln!("[gpu pds]   {l}");
            }
        }
        let task = pds::Task { if0: true, if1, temps };
        let mut host = TaskHost { mem: self.mem, common, unified, doutu: None, douti: Vec::new(), trace: self.trace };
        self.pds.trace = false;
        self.pds.run(&mut host, PDS_HEAP + code, &data, &task).map_err(|e| format!("PDS program {code:#x}: {e:?}"))?;
        Ok((host.doutu, host.douti))
    }

    /// Run a USC program on `insts`; returns its emitpix requests.
    fn usc(&mut self, u: pds::DoutU, coeffs: &mut [u32], shareds: &mut [u32], insts: &mut [Instance]) -> Result<Vec<(u32, u32)>, String> {
        let entry = USC_HEAP + u.exe_offset as u64;
        let mut m = usc::Machine { mem: self.mem, coeffs, shareds, cache: &mut self.cache, trace: self.trace, emits: Vec::new() };
        m.run(entry, insts).map_err(|f| format!("USC program {entry:#x}: fault at {:#x}: {}", f.pc, f.msg))?;
        Ok(m.emits)
    }
}

/// VDM_STATE: the vertex programs.
#[derive(Clone, Copy, Debug, Default)]
struct VdmState {
    vs_data: u64,
    vs_code: u64,
    out_size: u32,
    unified: u32,
    pds_data: u32,
    flatshade: u32,
}

impl Ppp {
    /// Apply a PPP state block: a header, then the words its presence bits announce, in
    /// Mesa's (fixed) order.
    fn apply(&mut self, w: &[u32]) -> Result<(), String> {
        let h = *w.first().ok_or("empty PPP block")?;
        let mut i = 1;
        let mut next = || -> Result<u32, String> {
            let v = *w.get(i).ok_or("PPP block too short")?;
            i += 1;
            Ok(v)
        };
        let bit = |n: u32| h & (1 << n) != 0;
        if bit(0) {
            self.ispctl = next()?;
            self.ispa = next()?;
        }
        if bit(2) {
            self.ispb = next()?;
        }
        if bit(3) {
            self.ispa_back = next()?;
        }
        if bit(4) {
            self.ispb_back = next()?;
        }
        if bit(5) {
            self.dbsc = next()?;
        }
        if bit(6) {
            for k in 0..4 {
                self.pds[k] = next()?;
            }
        }
        if bit(7) {
            self.varyingbase = next()?;
        }
        if bit(8) {
            self.texturedatabase = next()?;
        }
        if bit(9) {
            self.uniformdatabase = next()?;
        }
        if bit(10) {
            self.region_clip = [next()?, next()?];
        }
        if bit(11) {
            let n = ((h >> 12) & 0xf) + 1;
            self.viewports.clear();
            for _ in 0..n {
                let mut vp = [0f32; 6];
                for v in &mut vp {
                    *v = f(next()?);
                }
                self.viewports.push(vp);
            }
        }
        if bit(16) {
            self.wclamp = f(next()?);
        }
        if bit(17) {
            self.outsel = next()?;
        }
        for k in 0..3 {
            if bit(18 + k) {
                self.varying[k as usize] = next()?;
            }
        }
        if bit(21) {
            self.ppp_ctrl = next()?;
        }
        if bit(22) {
            next()?;
        }
        if bit(23) || bit(24) {
            return Err(format!("PPP stream-out program / context switch words not implemented (header {h:#x})"));
        }
        // bit 25: TERMINATE0/1 (the render's clip rectangle; the HWRT data has it too)
        Ok(())
    }
}

impl Gfx {
    /// A geometry job: walk the VDM control stream, shade the vertices, and add the
    /// primitives to the scene of the job's HWRT data.
    pub(crate) fn geom(&mut self, mem: &mut GpuMem, cmd: &[u32], trace: bool) -> Result<(), String> {
        let hwrt = cmd[1];
        let stream = u64_at(cmd, 16) & 0xff_ffff_fffc;
        let flags = cmd[80 / 4];
        if flags & 1 != 0 || !self.scenes.contains_key(&hwrt) {
            self.scenes.insert(hwrt, Scene::default());
        }
        let mut job = Job { mem, pds: Pds::default(), cache: HashMap::new(), trace };
        let mut ppp = Ppp::default();
        let mut vdm = VdmState::default();
        let mut shareds = vec![0u32; 1024];
        let mut pc = stream;
        let mut ret = Vec::new();
        let mut prims = Vec::new();
        for _ in 0..100_000 {
            let w0 = job.read(pc)?;
            match w0 >> 29 {
                // PPP_STATE: a state block in memory
                0 => {
                    let addr = ((w0 & 0xff) as u64) << 32 | (job.read(pc + 4)? & !3) as u64;
                    let n = ((w0 >> 8) & 0xff) as usize;
                    let words = job.words(addr, n)?;
                    ppp.apply(&words)?;
                    pc += 8;
                }
                // PDS_STATE: the vertex shaders' shared registers
                1 => {
                    let (w1, w2) = (job.read(pc + 4)?, job.read(pc + 8)?);
                    let data_words = 4 * (w0 & 0x3f) as usize;
                    let mut none = [];
                    let (kick, _) = job.pds((w2 & !0xf) as u64, (w1 & !0xf) as u64, data_words, vec![], false, &mut shareds, &mut none)?;
                    if let Some(u) = kick {
                        // a secondary program computing further shared registers
                        let mut insts = vec![Instance::new((u.temps as usize).max(8), 0)];
                        job.usc(u, &mut [], &mut shareds, &mut insts)?;
                    }
                    pc += 12;
                }
                // VDM_STATE
                2 => {
                    let mut p = pc + 4;
                    let mut next = |job: &mut Job| -> Result<u32, String> {
                        let v = job.read(p)?;
                        p += 4;
                        Ok(v)
                    };
                    if w0 & (1 << 28) != 0 {
                        next(&mut job)?;
                    }
                    if w0 & (1 << 27) != 0 {
                        vdm.vs_data = (next(&mut job)? & !0xf) as u64;
                    }
                    if w0 & (1 << 26) != 0 {
                        vdm.vs_code = (next(&mut job)? & !0xf) as u64;
                        vdm.out_size = next(&mut job)? & 0xff;
                        let w5 = next(&mut job)?;
                        vdm.unified = (w5 >> 10) & 0x3f;
                        vdm.pds_data = w5 & 0x3f;
                    }
                    if w0 & (7 << 22) != 0 {
                        return Err("tessellation / geometry shaders not implemented".into());
                    }
                    vdm.flatshade = (w0 >> 1) & 3;
                    pc = p;
                }
                // INDEX_LIST
                3 => {
                    let mut p = pc + 4;
                    let mut next = |job: &mut Job| -> Result<u32, String> {
                        let v = job.read(p)?;
                        p += 4;
                        Ok(v)
                    };
                    let bit = |n: u32| w0 & (1 << n) != 0;
                    let index_addr = if bit(28) { Some(((w0 & 0xff) as u64) << 32 | next(&mut job)? as u64) } else { None };
                    let count = if bit(27) { next(&mut job)? } else { 0 };
                    let instances = if bit(26) { next(&mut job)? + 1 } else { 1 };
                    let offset = if bit(25) { next(&mut job)? } else { 0 };
                    if bit(24) {
                        next(&mut job)?;
                        next(&mut job)?;
                    }
                    if bit(23) {
                        return Err("indirect draws not implemented".into());
                    }
                    if bit(22) {
                        next(&mut job)?;
                    }
                    let index_size = (w0 >> 17) & 3;
                    let topology = (w0 >> 8) & 0xf;
                    pc = p;
                    let mut idx = Vec::with_capacity(count as usize);
                    for i in 0..count {
                        let v = match index_addr {
                            None => i,
                            Some(a) => {
                                let bytes = 1u64 << index_size;
                                let at = a + i as u64 * bytes;
                                let word = job.read(at & !3)?;
                                match index_size {
                                    0 => (word >> (8 * (at & 3))) & 0xff,
                                    1 => (word >> (8 * (at & 2))) & 0xffff,
                                    _ => word,
                                }
                            }
                        };
                        idx.push(v.wrapping_add(offset));
                    }
                    for n in 0..instances {
                        let verts = shade_vertices(&mut job, &vdm, &mut shareds, &idx, n)?;
                        assemble(&verts, topology, &ppp, &mut prims)?;
                    }
                    self.draws += 1;
                }
                // STREAM_LINK
                4 => {
                    let target = ((w0 & 0xff) as u64) << 32 | (job.read(pc + 4)? & !3) as u64;
                    if w0 & (1 << 28) != 0 {
                        ret.push(pc + 8);
                    }
                    pc = target;
                }
                5 => pc = ret.pop().ok_or("VDM stream return without a call")?,
                6 => break,
                t => return Err(format!("VDM control block type {t} ({w0:#010x})")),
            }
        }
        self.prims += prims.len() as u64;
        if trace {
            eprintln!("[gpu geom] hwrt {hwrt:#x}: {} primitives", prims.len());
        }
        self.scenes.entry(hwrt).or_default().prims.extend(prims);
        Ok(())
    }
}

/// Run the vertex PDS program and the vertex shader for the vertices `idx` of instance `n`;
/// returns them in window coordinates (and their outputs).
fn shade_vertices(job: &mut Job, vdm: &VdmState, shareds: &mut [u32], idx: &[u32], n: u32) -> Result<Vec<Vertex>, String> {
    let unified_words = (4 * vdm.unified as usize).max(4);
    let mut insts = Vec::with_capacity(idx.len());
    let mut kick = None;
    for &i in idx {
        let mut unified = vec![0u32; unified_words];
        let (u, _) = job.pds(vdm.vs_code, vdm.vs_data, 4 * vdm.pds_data as usize, vec![i, n], false, shareds, &mut unified)?;
        kick = kick.or(u);
        let mut inst = Instance::new(8, 0);
        inst.vtxin = unified;
        inst.vout = vec![0; vdm.out_size as usize];
        insts.push(inst);
        job.trace = false;
    }
    let u = kick.ok_or("vertex PDS program without a DOUTU")?;
    for inst in &mut insts {
        inst.temps = vec![0; (u.temps as usize).max(8)];
    }
    job.usc(u, &mut [], shareds, &mut insts)?;
    Ok(insts.into_iter().map(|i| Vertex { win: [0.0; 4], out: i.vout }).collect())
}

/// Primitive assembly, clip-space rejection, the viewport transform and culling.
fn assemble(verts: &[Vertex], topology: u32, ppp: &Ppp, out: &mut Vec<Prim>) -> Result<(), String> {
    let tris: Vec<[usize; 3]> = match topology {
        6 => (0..verts.len() / 3).map(|t| [3 * t, 3 * t + 1, 3 * t + 2]).collect(),
        9 => (0..verts.len().saturating_sub(2)).map(|i| if i % 2 == 0 { [i, i + 1, i + 2] } else { [i + 1, i, i + 2] }).collect(),
        11 => (1..verts.len().saturating_sub(1)).map(|i| [0, i, i + 1]).collect(),
        t => return Err(format!("primitive topology {t} (points / lines / patches) not implemented")),
    };
    let vp = *ppp.viewports.first().ok_or("draw without a viewport")?;
    for t in tris {
        let mut v = [verts[t[0]].clone(), verts[t[1]].clone(), verts[t[2]].clone()];
        // no clipper: a primitive crossing w <= 0 is dropped (Vulkan's guard band covers the
        // rest; x/y outside the render are clipped per pixel)
        let mut ok = true;
        for vx in &mut v {
            let c: Vec<f32> = (0..4).map(|i| f(vx.out.get(i).copied().unwrap_or(0))).collect();
            let w = c[3].max(if ppp.ppp_ctrl & (1 << 5) != 0 { ppp.wclamp } else { f32::MIN });
            if w <= 0.0 {
                ok = false;
                break;
            }
            vx.win = [vp[0] + vp[1] * c[0] / w, vp[2] + vp[3] * c[1] / w, vp[4] + vp[5] * c[2] / w, 1.0 / w];
        }
        if !ok {
            continue;
        }
        let area = (v[1].win[0] - v[0].win[0]) * (v[2].win[1] - v[0].win[1]) - (v[2].win[0] - v[0].win[0]) * (v[1].win[1] - v[0].win[1]);
        // y points down: a positive area is clockwise on screen, counter-clockwise in
        // Vulkan's framebuffer coordinates
        let ccw = area > 0.0;
        match ppp.ppp_ctrl & 3 {
            1 if !ccw => continue,
            2 if ccw => continue,
            _ => {}
        }
        if area == 0.0 {
            continue;
        }
        out.push(Prim { v, ppp: ppp.clone(), back: !ccw });
    }
    Ok(())
}

/// A plane A*x + B*y + C through three (x, y, value) points.
fn plane(p: [[f32; 3]; 3]) -> [f32; 3] {
    let (x0, y0, f0) = (p[0][0], p[0][1], p[0][2]);
    let (dx1, dy1, df1) = (p[1][0] - x0, p[1][1] - y0, p[1][2] - f0);
    let (dx2, dy2, df2) = (p[2][0] - x0, p[2][1] - y0, p[2][2] - f0);
    let det = dx1 * dy2 - dx2 * dy1;
    if det == 0.0 {
        return [0.0, 0.0, f0];
    }
    let a = (df1 * dy2 - df2 * dy1) / det;
    let b = (dx1 * df2 - dx2 * df1) / det;
    [a, b, f0 - a * x0 - b * y0]
}

/// The PBE: write a tile's pixel output registers to memory, as `emitpix(state0, state1)`
/// with the render's PBE register words asks.
fn pbe(mem: &mut dyn usc::Memory, state: (u32, u32), regs: [u64; 3], tile: (u32, u32), out: &[[u32; 8]]) -> Result<(), String> {
    let (s0, s1) = state;
    if s1 & (1 << 6) != 0 {
        return Ok(());
    }
    let base = (s0 as u64) << 2 | ((s1 & 0x3f) as u64) << 34;
    let packmode = (s1 >> 7) & 0x3f;
    let source_pos = ((s1 >> 14) & 3) as usize;
    let source_format = (s1 >> 19) & 1;
    let (r0, r1) = (regs[0], regs[1]);
    let minclip_x = ((r0 >> 6) & 0x3fff) as u32;
    let swiz = [(r0 >> 20) & 7, (r0 >> 23) & 7, (r0 >> 26) & 7, (r0 >> 29) & 7];
    let memlayout = (r0 >> 32) & 3;
    let stride = (((r0 >> 34) & 0xffff) + 1) * 2;
    let maxclip_y = (r1 & 0x3fff) as u32;
    let size_y = (r1 >> 28) & 0xf;
    let maxclip_x = ((r1 >> 32) & 0x3fff) as u32;
    let minclip_y = ((r1 >> 46) & 0x3fff) as u32;
    let size_x = (r1 >> 60) & 0xf;
    // bytes per pixel, and whether channels are 8-bit fields of one register or registers
    let (bpp, chan_bytes) = match packmode {
        0x00 => (4, 1),
        0x23 => (2, 1),
        0x2f => (1, 1),
        0x34..=0x36 => (4, 4),
        0x28..=0x2a => (8, 4),
        0x1a | 0x1b | 0x1f => (12, 4),
        0x0b..=0x0d => (16, 4),
        p => return Err(format!("PBE pack mode {p:#x} not implemented")),
    };
    if chan_bytes == 1 && source_format != 1 {
        return Err("PBE 8-bit pack from F16 sources not implemented".into());
    }
    for py in 0..TILE {
        for px in 0..TILE {
            let (x, y) = (tile.0 + px, tile.1 + py);
            if x < minclip_x || x > maxclip_x || y < minclip_y || y > maxclip_y {
                continue;
            }
            let regs_px = &out[(py * TILE + px) as usize];
            let src = |c: usize| -> u32 {
                if chan_bytes == 1 {
                    (regs_px[source_pos] >> (8 * c)) & 0xff
                } else {
                    regs_px.get(source_pos + c).copied().unwrap_or(0)
                }
            };
            let nchan = bpp / chan_bytes;
            let mut bytes = Vec::with_capacity(bpp);
            for &sw in swiz.iter().take(nchan) {
                let v = match sw {
                    0..=3 => src(sw as usize),
                    4 => {
                        if chan_bytes == 1 {
                            0xff
                        } else {
                            1
                        }
                    }
                    _ => 0,
                };
                bytes.extend_from_slice(&v.to_le_bytes()[..chan_bytes]);
            }
            let off = match memlayout {
                0 => (y as u64 * stride + x as u64) * bpp as u64,
                1 => twiddle(x, y, 1 << size_x, 1 << size_y) * bpp as u64,
                l => return Err(format!("PBE memory layout {l} not implemented")),
            };
            let a = base + off;
            for (i, b) in bytes.iter().enumerate() {
                let at = a + i as u64;
                let w = mem.read32(at & !3).ok_or(format!("PBE write to unmapped {at:#x}"))?;
                let sh = 8 * (at & 3);
                mem.write32(at & !3, (w & !(0xff << sh)) | (*b as u32) << sh);
            }
        }
    }
    Ok(())
}

/// What a tile loop runs, from a fragment or transfer command.
struct Render {
    /// tile range: first tile and number of tiles in x and y
    origin: (u32, u32),
    tiles: (u32, u32),
    /// the render target size (pixels outside are never covered)
    size: (u32, u32),
    clear: [u32; 8],
    process_empty: bool,
    /// background object: (kick data, texture-state code, texture-state data and words,
    /// shared registers)
    bgnd: Option<(u64, u64, u64, usize, usize)>,
    /// end of tile: PDS code, data, data words, shared registers
    eot: (u64, u64, usize, usize),
    /// PBE register words per render target
    pbe: Vec<[u64; 3]>,
    depth_clear: f32,
    scissor_base: u64,
}

impl Render {
    fn run(&self, job: &mut Job, prims: &[Prim]) -> Result<u64, String> {
        let mut tiles = 0;
        for ty in self.origin.1..self.origin.1 + self.tiles.1 {
            for tx in self.origin.0..self.origin.0 + self.tiles.0 {
                let (x0, y0) = (tx * TILE, ty * TILE);
                let mine: Vec<&Prim> = prims.iter().filter(|p| overlaps(p, x0, y0)).collect();
                if mine.is_empty() && !self.process_empty {
                    continue;
                }
                let shaded = self.tile(job, (x0, y0), &mine)?;
                tiles += 1;
                // trace the first tile (with a shaded primitive, if there are any)
                if shaded || prims.is_empty() {
                    job.trace = false;
                }
            }
        }
        Ok(tiles)
    }

    fn pixels(&self, origin: (u32, u32)) -> Vec<Instance> {
        (0..TILE * TILE)
            .map(|i| {
                let (px, py) = (i % TILE, i / TILE);
                let mut inst = Instance::new(8, 0);
                inst.out = self.clear;
                inst.pos = [(origin.0 + px) as f32 + 0.5, (origin.1 + py) as f32 + 0.5, px as f32 + 0.5, py as f32 + 0.5];
                inst
            })
            .collect()
    }

    /// Render one tile; true if a primitive was shaded in it.
    fn tile(&self, job: &mut Job, origin: (u32, u32), prims: &[&Prim]) -> Result<bool, String> {
        let mut pixels = self.pixels(origin);
        let mut depth = vec![self.depth_clear; (TILE * TILE) as usize];
        if let Some((kick, tex_code, tex_data, tex_words, sh)) = self.bgnd {
            let mut shareds = vec![0u32; sh.max(64)];
            if tex_code != 0 {
                let mut none = [];
                job.pds(tex_code, tex_data, tex_words, vec![], false, &mut shareds, &mut none)?;
            }
            let mut none = [];
            let (u, _) = job.pds(kick + 32, kick, 8, vec![], false, &mut shareds, &mut none)?;
            let u = u.ok_or("background kick program without a DOUTU")?;
            for p in &mut pixels {
                p.temps = vec![0; (u.temps as usize).max(8)];
            }
            job.usc(u, &mut [], &mut shareds, &mut pixels)?;
        }
        let mut shaded = false;
        for prim in prims {
            shaded |= self.shade(job, origin, prim, &mut pixels, &mut depth)?;
        }
        let (code, data, words, sh) = self.eot;
        let mut shareds = vec![0u32; sh.max(16)];
        let mut none = [];
        let (u, _) = job.pds(code, data, words, vec![], false, &mut shareds, &mut none)?;
        let u = u.ok_or("end-of-tile program without a DOUTU")?;
        let mut eot = vec![Instance::new((u.temps as usize).max(8), 0)];
        let emits = job.usc(u, &mut [], &mut shareds, &mut eot)?;
        let out: Vec<[u32; 8]> = pixels.iter().map(|p| p.out).collect();
        for (s0, s1) in emits {
            let mrt = ((s1 >> 16) & 7) as usize;
            let regs = *self.pbe.get(mrt).ok_or(format!("emit to render target {mrt} without PBE words"))?;
            pbe(job.mem, (s0, s1), regs, origin, &out)?;
        }
        Ok(shaded)
    }

    /// Rasterise, depth-test and shade one primitive in one tile.
    fn shade(&self, job: &mut Job, origin: (u32, u32), prim: &Prim, pixels: &mut [Instance], depth: &mut [f32]) -> Result<bool, String> {
        let v = &prim.v;
        let ppp = &prim.ppp;
        let (ispa, _ispb) = if prim.back && ppp.ispctl & (1 << 19) != 0 { (ppp.ispa_back, ppp.ispb_back) } else { (ppp.ispa, ppp.ispb) };
        if (ispa >> 28) & 0xf != 0 {
            return Err(format!("ISP object type {} (lines / points) not implemented", (ispa >> 28) & 0xf));
        }
        let scissor = if ppp.ispctl & (1 << 16) != 0 {
            let at = self.scissor_base + 8 * (ppp.dbsc & 0xffff) as u64;
            let (w0, w1) = (job.read(at)?, job.read(at + 4)?);
            Some((w0 >> 16, w0 & 0xffff, w1 >> 16, w1 & 0xffff))
        } else {
            None
        };
        let zplane = plane([[v[0].win[0], v[0].win[1], v[0].win[2]], [v[1].win[0], v[1].win[1], v[1].win[2]], [v[2].win[0], v[2].win[1], v[2].win[2]]]);
        // edge functions, oriented so that inside is positive
        let area = (v[1].win[0] - v[0].win[0]) * (v[2].win[1] - v[0].win[1]) - (v[2].win[0] - v[0].win[0]) * (v[1].win[1] - v[0].win[1]);
        let s = area.signum();
        let edge = |a: &Vertex, b: &Vertex, x: f32, y: f32| s * ((b.win[0] - a.win[0]) * (y - a.win[1]) - (b.win[1] - a.win[1]) * (x - a.win[0]));
        let dcmp = (ispa >> 17) & 7;
        let dwrite = ispa & (1 << 21) == 0;
        let mut covered = Vec::new();
        for (i, px) in pixels.iter().enumerate() {
            let (x, y) = (px.pos[0], px.pos[1]);
            let (ix, iy) = (x as u32, y as u32);
            if ix >= self.size.0 || iy >= self.size.1 {
                continue;
            }
            if let Some((xmin, xmax, ymin, ymax)) = scissor {
                if ix < xmin || ix >= xmax || iy < ymin || iy >= ymax {
                    continue;
                }
            }
            let (e0, e1, e2) = (edge(&v[1], &v[2], x, y), edge(&v[2], &v[0], x, y), edge(&v[0], &v[1], x, y));
            if e0 < 0.0 || e1 < 0.0 || e2 < 0.0 {
                continue;
            }
            let z = zplane[0] * x + zplane[1] * y + zplane[2];
            let pass = match dcmp {
                0 => false,
                1 => z < depth[i],
                2 => z == depth[i],
                3 => z <= depth[i],
                4 => z > depth[i],
                5 => z != depth[i],
                6 => z >= depth[i],
                _ => true,
            };
            if !pass {
                continue;
            }
            if dwrite {
                depth[i] = z;
            }
            covered.push(i);
        }
        // tag write disabled: depth / stencil only
        if covered.is_empty() || ppp.ispctl & (1 << 21) != 0 {
            return Ok(false);
        }
        let size1 = ppp.pds[2];
        let size2 = ppp.pds[3];
        // coefficients: the coefficient program's DOUTIs, planes computed by the TSP
        let mut coeffs = vec![0u32; (16 * ((size1 >> 4) & 0x3f) as usize).max(16)];
        let vwords = ((size1 >> 10) & 0x3f) as u64 * 4;
        if ppp.varyingbase != 0 && vwords != 0 {
            let data = (ppp.varyingbase & !0xf) as u64;
            let mut none = [];
            let mut none2 = [];
            let (_, douti) = job.pds(data + 4 * vwords, data, vwords as usize, vec![], false, &mut none, &mut none2)?;
            for d in douti {
                tsp(d, prim, origin, &mut coeffs)?;
            }
        }
        let mut shareds = vec![0u32; (16 * (size2 >> 23) as usize).max(64)];
        let unicode = (ppp.pds[1] & !0xf) as u64;
        if unicode != 0 {
            let words = 4 * ((size1 >> 23) & 0x1ff) as usize;
            let mut none = [];
            let (u, _) = job.pds(unicode, (ppp.uniformdatabase & !0xf) as u64, words, vec![], false, &mut shareds, &mut none)?;
            if let Some(u) = u {
                let mut insts = vec![Instance::new((u.temps as usize).max(8), 0)];
                job.usc(u, &mut [], &mut shareds, &mut insts)?;
            }
        }
        let kick = (ppp.pds[0] & !0xf) as u64;
        let mut none = [];
        let mut none2 = [];
        let (u, _) = job.pds(kick + 32, kick, 8, vec![], false, &mut none, &mut none2)?;
        let u = u.ok_or("fragment kick program without a DOUTU")?;
        let mut insts: Vec<Instance> = covered
            .iter()
            .map(|&i| {
                let mut inst = pixels[i].clone();
                inst.temps = vec![0; (u.temps as usize).max(8)];
                inst.feedback = false;
                inst
            })
            .collect();
        job.usc(u, &mut coeffs, &mut shareds, &mut insts)?;
        // punch-through and depth-feedback objects keep only the pixels the shader passed
        let feedback = matches!((ispa >> 24) & 7, 2 | 4 | 5);
        for (inst, &i) in insts.iter().zip(&covered) {
            if !feedback || inst.feedback {
                pixels[i].out = inst.out;
            }
        }
        Ok(true)
    }
}

fn overlaps(p: &Prim, x0: u32, y0: u32) -> bool {
    let (mut minx, mut maxx, mut miny, mut maxy) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
    for v in &p.v {
        minx = minx.min(v.win[0]);
        maxx = maxx.max(v.win[0]);
        miny = miny.min(v.win[1]);
        maxy = maxy.max(v.win[1]);
    }
    maxx >= x0 as f32 && minx <= (x0 + TILE) as f32 && maxy >= y0 as f32 && miny <= (y0 + TILE) as f32
}

/// The TSP's part of a DOUTI: plane coefficients for one iterator, written as (A, B, C, 0)
/// per component at coefficient set `dest` (128-bit units).
///
/// The TSP vertex holds [1/w if OUTPUT_SEL.rhw_pres][z if tsp_unclamped_z_pres][the vertex
/// outputs from 4 on]; `f32_offset` (16-bit units) picks the first component. Perspective
/// iterators interpolate value/w (the shader divides by the 1/w plane); flat ones use the
/// provoking vertex's value.
fn tsp(douti: u64, prim: &Prim, origin: (u32, u32), coeffs: &mut [u32]) -> Result<(), String> {
    let src = douti as u32;
    let dest = ((douti >> 54) & 0xff) as usize;
    let shademodel = (src >> 24) & 3;
    let size = ((src >> 18) & 3) as usize + 1;
    let perspective = src & (1 << 16) != 0;
    let first = ((src >> 8) & 0xff) as usize / 2;
    if src & ((1 << 27) | (1 << 26) | (1 << 23) | (1 << 17)) != 0 {
        return Err(format!("DOUTI with depth bias / primitive ID / point sprite / f16 ({src:#x}) not implemented"));
    }
    let outsel = prim.ppp.outsel;
    let tsp_vertex = |v: &Vertex| -> Vec<f32> {
        let mut t = Vec::new();
        if outsel & (1 << 16) != 0 {
            t.push(v.win[3]);
        }
        if outsel & (1 << 21) != 0 {
            t.push(v.win[2]);
        }
        t.extend(v.out.iter().skip(4).map(|&w| f(w)));
        t
    };
    let tv: Vec<Vec<f32>> = prim.v.iter().map(tsp_vertex).collect();
    for c in 0..size {
        let a = first + c;
        let val = |k: usize| tv[k].get(a).copied().unwrap_or(0.0);
        let p = match shademodel {
            3 => {
                let pts: [[f32; 3]; 3] = std::array::from_fn(|k| {
                    let v = &prim.v[k];
                    let x = val(k) * if perspective { v.win[3] } else { 1.0 };
                    [v.win[0], v.win[1], x]
                });
                plane(pts)
            }
            k => [0.0, 0.0, val(k as usize)],
        };
        // relative to the tile: A*(x - x0) + B*(y - y0) + C'
        let c0 = p[2] + p[0] * origin.0 as f32 + p[1] * origin.1 as f32;
        let at = 4 * (dest + c);
        let slot = coeffs.get_mut(at..at + 4).ok_or(format!("coefficient set {} beyond the store", dest + c))?;
        slot.copy_from_slice(&[p[0].to_bits(), p[1].to_bits(), c0.to_bits(), 0]);
    }
    Ok(())
}

impl Gfx {
    /// A fragment job: render the scene of its HWRT data.
    pub(crate) fn frag(&mut self, mem: &mut GpuMem, cmd: &[u32], screen: u32, trace: bool) -> Result<(), String> {
        let hwrt = cmd[1];
        let scene = self.scenes.remove(&hwrt).unwrap_or_default();
        let (w, h) = ((screen & 0x7fff) + 1, ((screen >> 16) & 0x7fff) + 1);
        let r = |off: usize| cmd[(16 + off) / 4];
        let r64 = |off: usize| u64_at(cmd, 16 + off);
        let out_ctrl = r(0);
        if out_ctrl & 3 == 2 {
            return Err("8 output registers per pixel not implemented".into());
        }
        let mut clear = [0u32; 8];
        for (i, c) in clear.iter_mut().enumerate() {
            *c = r(4 + 4 * i);
        }
        let bgobjvals = r(40);
        let isp_aa = r(44);
        if isp_aa & 3 != 0 {
            return Err("multisampled renders not implemented".into());
        }
        let isp_ctl = r(52);
        let pixel_info = r(60);
        let pixel_data = (r(72) & !0xf) as u64;
        if r64(120) & ((1 << 15) | (1 << 14) | (1 << 19) | (1 << 18) | (1 << 1) | (1 << 2)) != 0 {
            return Err(format!("depth / stencil load and store (ZLSCTL {:#x}) not implemented", r64(120)));
        }
        let pbe: Vec<[u64; 3]> = (0..8).map(|i| [r64(152 + 24 * i), r64(160 + 24 * i), r64(168 + 24 * i)]).collect();
        let bg = [r64(352), r64(360), r64(368)];
        let bgnd = (bgobjvals & (1 << 9) != 0).then(|| {
            let kick = bg[0] & 0xffff_fff0;
            let tex_code = ((bg[0] >> 36) & 0xfff_ffff) << 4;
            let tex_data = ((bg[1] >> 36) & 0xfff_ffff) << 4;
            let tex_words = 4 * ((bg[2] >> 16) & 0x7f) as usize;
            let sh = 16 * ((bg[2] >> 55) & 0x1ff) as usize;
            (kick, tex_code, tex_data, tex_words, sh)
        });
        let render = Render {
            origin: (0, 0),
            tiles: (w.div_ceil(TILE), h.div_ceil(TILE)),
            size: (w, h),
            clear,
            process_empty: isp_ctl & (1 << 17) != 0,
            bgnd,
            // the end-of-tile PDS code sits in the EOT static data area at PDS offset 0x80
            eot: (0x80, pixel_data, 4 * (pixel_info & 0x1f) as usize, 16 * ((pixel_info >> 9) & 0x3f) as usize),
            pbe,
            depth_clear: f(r(36)),
            scissor_base: r64(96) & 0xff_ffff_fffc,
        };
        let mut job = Job { mem, pds: Pds::default(), cache: HashMap::new(), trace };
        let tiles = render.run(&mut job, &scene.prims)?;
        self.tiles += tiles;
        if trace {
            eprintln!("[gpu frag] hwrt {hwrt:#x}: {w}x{h}, {} primitives, {tiles} tiles", scene.prims.len());
        }
        Ok(())
    }

    /// A transfer (TQ_3D) job: a render of background objects only, with the region and
    /// PBE state in the command.
    pub(crate) fn transfer(&mut self, mem: &mut GpuMem, cmd: &[u32], trace: bool) -> Result<(), String> {
        let r = |off: usize| cmd[(8 + off) / 4];
        let r64 = |off: usize| u64_at(cmd, 8 + off);
        let bgobjvals = r(0);
        let isp_render = r(56);
        if isp_render & 3 != 3 {
            return Err(format!("transfer render mode {} (only FAST_SCALE is implemented)", isp_render & 3));
        }
        if r(4) & 3 == 2 {
            return Err("8 output registers per pixel not implemented".into());
        }
        let mut clear = [0u32; 8];
        for (i, c) in clear.iter_mut().take(4).enumerate() {
            *c = r(8 + 4 * i);
        }
        let (size, origin) = (r(24), r(28));
        let screen = r(64);
        let pixel_info = r(44);
        let bg = [r64(72), r64(80), r64(88)];
        let bgnd = (bgobjvals & (1 << 9) != 0).then(|| {
            let kick = bg[0] & 0xffff_fff0;
            let tex_code = ((bg[0] >> 36) & 0xfff_ffff) << 4;
            let tex_data = ((bg[1] >> 36) & 0xfff_ffff) << 4;
            let tex_words = 4 * ((bg[2] >> 16) & 0x7f) as usize;
            let sh = 16 * ((bg[2] >> 55) & 0x1ff) as usize;
            (kick, tex_code, tex_data, tex_words, sh)
        });
        // Mesa packs two PBE register words per render target for transfers
        let pbe: Vec<[u64; 3]> = (0..4).map(|i| [r64(104 + 16 * i), r64(112 + 16 * i), 0]).collect();
        let render = Render {
            origin: ((origin >> 16) & 0x3ff, origin & 0x3ff),
            tiles: ((size >> 16) & 0x3ff, size & 0x3ff),
            size: ((screen & 0x7fff) + 1, ((screen >> 16) & 0x7fff) + 1),
            clear,
            process_empty: true,
            bgnd,
            eot: ((r(48) & !0xf) as u64, (r(52) & !0xf) as u64, 4 * (pixel_info & 0x1f) as usize, 16 * ((pixel_info >> 9) & 0x3f) as usize),
            pbe,
            depth_clear: 1.0,
            scissor_base: 0,
        };
        let mut job = Job { mem, pds: Pds::default(), cache: HashMap::new(), trace };
        let tiles = render.run(&mut job, &[])?;
        self.tiles += tiles;
        if trace {
            eprintln!("[gpu tq] {tiles} tiles from tile {:?}", render.origin);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mem(HashMap<u64, u32>);

    impl usc::Memory for Mem {
        fn read32(&mut self, va: u64) -> Option<u32> {
            Some(self.0.get(&va).copied().unwrap_or(0xeeee_eeee))
        }
        fn write32(&mut self, va: u64, v: u32) -> bool {
            self.0.insert(va, v);
            true
        }
    }

    /// One tile of output registers: pixel (x, y) holds x | y << 8 | 0xff << 24 in o0.
    fn tile_regs() -> Vec<[u32; 8]> {
        (0..256).map(|i| [(i % 16) | (i / 16) << 8 | 0xff << 24, 0, 0, 0, 0, 0, 0, 0]).collect()
    }

    #[test]
    fn pbe_writes_a_twiddled_rgba8_tile() {
        // Mesa's eot1.imm state for the 64x64 RGBA8 attachment: address 0x80_000a_d000,
        // U8U8U8U8, norm, 8-bit source
        let (s0, s1) = (0x2b400, 0x82020);
        // identity swizzle, twiddled, clip 0..63 x 0..63, 64 x 64 (log2 6)
        let r0 = (1u64 << 32) | (3 << 29) | (2 << 26) | (1 << 23);
        let r1 = (6u64 << 60) | (63 << 32) | (6 << 28) | 63;
        let mut mem = Mem(HashMap::new());
        pbe(&mut mem, (s0, s1), [r0, r1, 0], (16, 32), &tile_regs()).unwrap();
        let base = 0x80_000a_d000u64;
        // pixel (16 + 3, 32 + 5)
        let at = base + 4 * twiddle(19, 37, 64, 64);
        assert_eq!(mem.0[&at], 3 | 5 << 8 | 0xff << 24);
        // outside the tile: untouched
        assert!(!mem.0.contains_key(&(base + 4 * twiddle(0, 0, 64, 64))));
    }

    #[test]
    fn pbe_writes_a_linear_u32_rectangle_within_the_clip() {
        // U32 (0x34), 8-bit source flag as Mesa sets for raw copies, address 0x1000
        let (s0, s1) = ((0x1000u64 >> 2) as u32, (0x34 << 7) | (1 << 19));
        // swizzle chan0, zero, zero, one; linear, stride 8 px; clip x 2..5, y 0..1
        let r0 = (3u64 << 34) | (4 << 29) | (5 << 26) | (5 << 23) | (2 << 6);
        let r1 = (5u64 << 32) | 1;
        let mut mem = Mem(HashMap::new());
        pbe(&mut mem, (s0, s1), [r0, r1, 0], (0, 0), &tile_regs()).unwrap();
        assert_eq!(mem.0.len(), 8);
        assert_eq!(mem.0[&(0x1000 + 4 * (8 + 3))], 3 | 1 << 8 | 0xff << 24);
    }

    #[test]
    fn plane_through_three_points() {
        let p = plane([[0.0, 0.0, 1.0], [10.0, 0.0, 11.0], [0.0, 10.0, -9.0]]);
        assert_eq!(p, [1.0, -1.0, 1.0]);
    }

    #[test]
    fn ppp_block_in_mesa_order() {
        let mut p = Ppp::default();
        // header with ispctl (+ispa), viewport (1) and ppp_ctrl
        let h = 1 | (1 << 11) | (1 << 21);
        let vp = [32.0f32, 32.0, 32.0, 32.0, 0.0, 1.0].map(f32::to_bits);
        let mut w = vec![h, 0x11, 0x22];
        w.extend_from_slice(&vp);
        w.push(0x2a0);
        p.apply(&w).unwrap();
        assert_eq!((p.ispctl, p.ispa, p.ppp_ctrl), (0x11, 0x22, 0x2a0));
        assert_eq!(p.viewports, vec![[32.0, 32.0, 32.0, 32.0, 0.0, 1.0]]);
    }
}
