//! Job capture for `vc4-replay` (`SBC_VC4_CAPTURE=dir` appends to `dir/jobs.bin`): each job
//! with the RAM pages it touched, as they were before it ran, so it can run again on the host
//! without the guest.
//!
//! Record: `"VC4J"`, kind (u8: 0 bin, 1 render), start, end, semaphore (u32), page count (u32),
//! then per page its address (u32) and 4096 bytes. Addresses are RAM offsets (bus addresses
//! without the alias bits).

use crate::mem::{Pages, PAGE};
use crate::Job;

pub fn append(dir: &str, job: &Job, sem: u32, pages: &Pages) {
    use std::io::Write;
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(format!("{dir}/jobs.bin")) else {
        crate::log_first(&format!("vc4: cannot write {dir}/jobs.bin"));
        return;
    };
    let mut out = Vec::with_capacity(21 + pages.len() * (PAGE + 4));
    out.extend_from_slice(b"VC4J");
    let (kind, start, end) = match *job { Job::Bin { start, end } => (0u8, start, end), Job::Render { start, end } => (1, start, end) };
    out.push(kind);
    for v in [start, end, sem, pages.len() as u32] { out.extend_from_slice(&v.to_le_bytes()); }
    for (a, d) in pages {
        out.extend_from_slice(&a.to_le_bytes());
        out.extend_from_slice(d);
    }
    let _ = f.write_all(&out);
}

/// A captured job.
pub struct Captured {
    pub job: Job,
    pub sem: u32,
    pub pages: Vec<(u32, Vec<u8>)>,
}

/// Parse a capture file.
pub fn parse(data: &[u8]) -> Result<Vec<Captured>, String> {
    let mut v = Vec::new();
    let mut p = 0;
    let u32_at = |p: usize| u32::from_le_bytes(data[p..p + 4].try_into().unwrap());
    while p < data.len() {
        if p + 21 > data.len() || &data[p..p + 4] != b"VC4J" { return Err(format!("bad record at {p}")); }
        let kind = data[p + 4];
        let (start, end, sem, n) = (u32_at(p + 5), u32_at(p + 9), u32_at(p + 13), u32_at(p + 17) as usize);
        p += 21;
        let mut pages = Vec::with_capacity(n);
        for _ in 0..n {
            if p + 4 + PAGE > data.len() { return Err("truncated page".into()); }
            pages.push((u32_at(p), data[p + 4..p + 4 + PAGE].to_vec()));
            p += 4 + PAGE;
        }
        let job = if kind == 0 { Job::Bin { start, end } } else { Job::Render { start, end } };
        v.push(Captured { job, sem, pages });
    }
    Ok(v)
}

/// Replay captured jobs on `ram` through the device's registers, as the kernel would: overflow
/// memory for the binner comes from the top 8 MiB of `ram`. Pages a replayed job wrote keep
/// their replayed contents (a later job's captured copy of them is the original run's).
/// Returns the frame each rendering job stored.
pub fn replay(jobs: &[Captured], ram: &mut [u8]) -> Vec<crate::tlb::Frame> {
    replay_counted(jobs, ram).0
}

/// [`replay`], and the QPU instructions the jobs ran.
pub fn replay_counted(jobs: &[Captured], ram: &mut [u8]) -> (Vec<crate::tlb::Frame>, u64) {
    use crate::*;
    let mut v = V3d::new();
    v.time_scale = 0.0;
    v.exec().track = Some(Default::default());
    v.tracking = true;
    let mut frames = Vec::new();
    let overflow = (ram.len() as u32).saturating_sub(8 << 20);
    let mut next = overflow;
    for c in jobs {
        let written = v.exec().track.clone().unwrap_or_default();
        for (a, d) in &c.pages {
            let a = *a as usize;
            if a + PAGE <= ram.len() && !written.contains(&(a as u32)) { ram[a..a + PAGE].copy_from_slice(d); }
        }
        match c.job {
            Job::Bin { start, end } => {
                v.write(CT0CA as u64, start, ram);
                v.write(CT0EA as u64, end, ram);
                let mut tries = 0;
                while v.busy() && tries < 64 {
                    tries += 1;
                    if next + (512 << 10) > ram.len() as u32 { next = overflow; }
                    v.write(BPOA as u64, next, ram);
                    v.write(BPOS as u64, 512 << 10, ram);
                    next += 512 << 10;
                }
            }
            Job::Render { start, end } => {
                v.write(CT1CA as u64, start, ram);
                v.write(CT1EA as u64, end, ram);
                // the configuration is among the first packets (the kernel may put the clear first)
                let mut m = crate::mem::Mem::new(ram);
                let mut a = start;
                for _ in 0..8 {
                    let op = m.r8(a);
                    if op == crate::cl::TILE_RENDERING_MODE_CONFIG {
                        let mut p = [0u8; 11];
                        m.read(a, &mut p);
                        frames.push(crate::tlb::Frame::parse(&p));
                        break;
                    }
                    match crate::cl::size(op) { Some(n) => a = a.wrapping_add(n), None => break }
                }
            }
        }
        v.write(INTCTL as u64, !0, ram);
    }
    let insns = v.exec().insns + v.user.insns;
    (frames, insns)
}

/// A frame (as `TILE_RENDERING_MODE_CONFIG` describes it) as a binary PPM.
pub fn ppm(f: &crate::tlb::Frame, mem: &mut crate::mem::Mem) -> Vec<u8> {
    let (w, h) = (f.width.max(1), f.height.max(1));
    let rgba = f.format == 1;
    let lay = crate::tiling::Layout::new(f.tiling, if rgba { 4 } else { 2 }, w);
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    for y in 0..h {
        for x in 0..w {
            let a = f.addr.wrapping_add(lay.offset(x, y));
            let c = if rgba { mem.r32(a) } else { crate::tlb::from565(mem.r16(a)) }.to_le_bytes();
            out.extend_from_slice(&c[..3]);
        }
    }
    out
}
