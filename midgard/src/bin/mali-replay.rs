//! Replay job chains captured with `SBC_MALI_CAPTURE=dir` on the host, without the guest:
//!   mali-replay dir/chains.bin [first] [count]
//! Each chain's pages are mapped at their GPU addresses through synthetic page tables and
//! the chain runs as on the emulated GPU; with SBC_MALI_DUMP=dir the fragment jobs' render
//! targets are written as PPMs, with SBC_MALI_LOG=1 each job is traced.

use midgard::job::JobState;
use midgard::mmu::AddressSpace;
use midgard::vm::Vm;
use std::collections::BTreeMap;

const PAGE: usize = 4096;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1]).expect("read capture");
    let first: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    let count: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);
    let trace = std::env::var("SBC_MALI_LOG").is_ok();
    let mut chains = Vec::new();
    let mut p = 0;
    while p + 12 <= data.len() {
        let head = u64::from_le_bytes(data[p..p + 8].try_into().unwrap());
        let n = u32::from_le_bytes(data[p + 8..p + 12].try_into().unwrap()) as usize;
        p += 12;
        let mut pages = BTreeMap::new();
        for _ in 0..n {
            let va = u64::from_le_bytes(data[p..p + 8].try_into().unwrap());
            pages.insert(va, data[p + 8..p + 8 + PAGE].to_vec());
            p += 8 + PAGE;
        }
        chains.push((head, pages));
    }
    eprintln!("{} chains", chains.len());
    // One persistent GPU memory: later chains see earlier chains' writes, and each chain's
    // captured pages (its view at submission) are loaded first.
    let mut mem: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let mut jobs = JobState::default();
    // Pages the replayed GPU wrote keep their replayed content: a capture taken with an
    // older model would otherwise bring back that model's results.
    let mut gpu_written = std::collections::BTreeSet::new();
    for (i, (head, pages)) in chains.iter().enumerate() {
        for (va, d) in pages {
            let page = mem.entry(*va).or_insert_with(|| d.clone());
            for c in 0..PAGE / 16 {
                if !gpu_written.contains(&(va + 16 * c as u64)) {
                    page[16 * c..16 * c + 16].copy_from_slice(&d[16 * c..16 * c + 16]);
                }
            }
        }
        if i < first || i - first >= count {
            continue;
        }
        // Build RAM: data pages, then four-level tables mapping each VA page.
        let vas: Vec<u64> = mem.keys().cloned().collect();
        let mut ram = vec![0u8; (vas.len() + 1) * PAGE];
        for (k, va) in vas.iter().enumerate() {
            ram[(k + 1) * PAGE..(k + 2) * PAGE].copy_from_slice(&mem[va]);
        }
        let mut tables: BTreeMap<(u32, u64), usize> = BTreeMap::new();
        let alloc = |ram: &mut Vec<u8>| {
            let at = ram.len();
            ram.resize(at + PAGE, 0);
            at
        };
        let root = alloc(&mut ram);
        for (k, va) in vas.iter().enumerate() {
            let mut table = root;
            for level in 0..3u32 {
                let idx = ((va >> (39 - 9 * level)) & 0x1ff) as usize;
                let key = (level, va >> (39 - 9 * level));
                let next = match tables.get(&key) {
                    Some(&t) => t,
                    None => {
                        let t = alloc(&mut ram);
                        tables.insert(key, t);
                        ram[table + 8 * idx..table + 8 * idx + 8].copy_from_slice(&(t as u64 | 3).to_le_bytes());
                        t
                    }
                };
                table = next;
            }
            let idx = ((va >> 12) & 0x1ff) as usize;
            let pte = ((k + 1) * PAGE) as u64 | 1 | 3 << 6;
            ram[table + 8 * idx..table + 8 * idx + 8].copy_from_slice(&pte.to_le_bytes());
        }
        let aspace = AddressSpace { active_transtab: root as u64 | 3, ..Default::default() };
        let mut vm = Vm::new(&mut ram, 0, aspace);
        let t = std::time::Instant::now();
        let r = jobs.run_chain(&mut vm, *head, 0, trace);
        eprintln!("chain {i} head {head:#x}: status {:#x}, {} cycles, {:.1} ms, fault {:?}", r.status, r.cycles, t.elapsed().as_secs_f64() * 1e3, vm.fault);
        // keep the chain's writes
        for (k, va) in vas.iter().enumerate() {
            let new = &ram[(k + 1) * PAGE..(k + 2) * PAGE];
            let old = mem.get_mut(va).unwrap();
            for c in 0..PAGE / 16 {
                // the job header's status chunk is written back on every run
                if old[16 * c..16 * c + 16] != new[16 * c..16 * c + 16] && !(c % 4 == 0 && new[16 * c] == 1) {
                    gpu_written.insert(va + 16 * c as u64);
                }
            }
            old.copy_from_slice(new);
        }
    }
}
