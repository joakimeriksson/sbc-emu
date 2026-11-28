//! Replay VideoCore IV 3D jobs captured with `SBC_VC4_CAPTURE=dir` on the host, without the
//! guest:
//!   vc4-replay dir/jobs.bin [first] [count]
//! Jobs before `first` only set up memory. With `SBC_VC4_DUMP=dir` each frame is written as a
//! PPM; `SBC_VC4_LOG=1`, `SBC_VC4_CL=1`, `SBC_VC4_TRACE=N` trace as in the emulator.

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(path) = args.get(1) else {
        eprintln!("usage: vc4-replay dir/jobs.bin [first] [count]");
        std::process::exit(2);
    };
    let data = std::fs::read(path).unwrap_or_else(|e| { eprintln!("{path}: {e}"); std::process::exit(1) });
    let jobs = vc4_3d::capture::parse(&data).unwrap_or_else(|e| { eprintln!("{path}: {e}"); std::process::exit(1) });
    let first: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    let count: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);
    let pages: usize = jobs.iter().map(|j| j.pages.len()).sum();
    eprintln!("{} jobs, {} pages", jobs.len(), pages);
    // RAM up to the highest page captured, plus room for the binner's overflow memory
    let top = jobs.iter().flat_map(|j| j.pages.iter().map(|p| p.0 as usize + 4096)).max().unwrap_or(0);
    let mut ram = vec![0u8; top + (16 << 20)];
    let sel: Vec<vc4_3d::capture::Captured> = jobs.into_iter().enumerate()
        .filter(|(k, _)| *k < first.saturating_add(count))
        .map(|(_, j)| j).collect();
    let t = std::time::Instant::now();
    let frames = vc4_3d::capture::replay(&sel, &mut ram);
    eprintln!("replayed in {:.1} ms; {} frames", t.elapsed().as_secs_f64() * 1e3, frames.len());
    for f in &frames {
        eprintln!("  frame at {:#010x}: {}x{} {} {}", f.addr, f.width, f.height, if f.format == 1 { "RGBA8888" } else { "BGR565" },
                  ["raster", "T", "LT", "?"][f.tiling as usize & 3]);
    }
}
