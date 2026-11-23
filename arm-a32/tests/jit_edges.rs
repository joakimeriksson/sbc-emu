//! The JIT against the interpreter on targeted cases (from the review of the x86-64
//! backend): division and CLZ edges, LDM/STM and stores with the base in the list,
//! interworking PC writes in A32 and Thumb, cross-page chaining in User, SVC and System
//! mode, Thumb control flow, faults at the end of RAM, self-modifying code, page straddles
//! and small chaining budgets; and the random cases of `jit_diff.rs` with chaining budgets
//! and in other modes. `JIT_DIFF_CASES` sets the number of random cases.
#[path = "common/jit_gen.rs"]
mod jit_gen;
use arm_a32::{CodeWatch, Cpu};
use emu_core::{Bus, Core, FlatRam};
use jit_gen::*;

const BKPT: u32 = 0xe120_0070;

struct Case {
    arm: Vec<(u32, u32)>,
    thumb: Vec<(u32, u16)>,
    regs: Vec<(usize, u32)>,
    cpsr: u32,
    data: Vec<(u32, u32)>,
    chain: u64,
}

fn case() -> Case {
    Case { arm: vec![], thumb: vec![], regs: vec![], cpsr: 0x1d3, data: vec![], chain: u64::MAX }
}

fn mk(c: &Case) -> (Cpu, FlatRam) {
    let mut ram = FlatRam::new(0, RAM);
    for &(a, w) in &c.arm {
        ram.write32(a as u64, w).unwrap();
    }
    for &(a, h) in &c.thumb {
        ram.write16(a as u64, h).unwrap();
    }
    for &(a, w) in &c.data {
        ram.write32(a as u64, w).unwrap();
    }
    let mut cpu = Cpu::new();
    cpu.cpsr = c.cpsr;
    for &(r, v) in &c.regs {
        cpu.r[r] = v;
    }
    (cpu, ram)
}

fn go(cpu: &mut Cpu, ram: &mut FlatRam) -> String {
    for _ in 0..5000 {
        match cpu.step(ram) {
            Ok(()) => {}
            Err(emu_core::Trap::Brk(_)) => return "brk".into(),
            Err(t) => return format!("{t:?} at {:#x}", cpu.r[15]),
        }
    }
    "timeout".into()
}

fn cmp(c: &Case) -> Result<(), String> {
    let (mut a, mut ra) = mk(c);
    let (mut b, mut rb) = mk(c);
    let ea = go(&mut a, &mut ra);
    // SAFETY: rb outlives b
    unsafe { b.set_ram(rb.mem.as_mut_ptr(), 0, RAM as u32) };
    if !b.enable_jit(CodeWatch::new(0, RAM as u32)) {
        return Err("no JIT".into());
    }
    b.jit_set_hot(1);
    b.chain_until = c.chain;
    let eb = go(&mut b, &mut rb);
    let mut err = String::new();
    if ea != eb {
        err += &format!("end: interp {ea}, jit {eb}; ");
    }
    if a.r != b.r {
        err += &format!("regs: interp {:08x?} jit {:08x?}; ", a.r, b.r);
    }
    if a.cpsr != b.cpsr {
        err += &format!("cpsr interp {:08x} jit {:08x}; ", a.cpsr, b.cpsr);
    }
    if a.instret() != b.instret() {
        err += &format!("instret interp {} jit {}; ", a.instret(), b.instret());
    }
    if ra.mem != rb.mem {
        let at = ra.mem.iter().zip(&rb.mem).position(|(x, y)| x != y).unwrap();
        err += &format!("mem at {at:#x}; ");
    }
    if b.jit_stats().unwrap().0 == 0 {
        err += "nothing compiled; ";
    }
    if err.is_empty() { Ok(()) } else { Err(err) }
}

fn arm_at(base: u32, w: &[u32]) -> Vec<(u32, u32)> {
    w.iter().enumerate().map(|(i, &x)| (base + 4 * i as u32, x)).collect()
}
fn th_at(base: u32, h: &[u16]) -> Vec<(u32, u16)> {
    h.iter().enumerate().map(|(i, &x)| (base + 2 * i as u32, x)).collect()
}

fn report(fails: &mut Vec<String>, name: &str, r: Result<(), String>) {
    if let Err(e) = r {
        fails.push(format!("{name}: {e}"));
    }
}

#[test]
fn data_memory_and_interworking_edges() {
    if !jit_ready("data_memory_and_interworking_edges") {
        return;
    }
    let mut fails = Vec::new();
    let vals = [0u32, 1, 2, 7, 0xffff_ffff, 0xffff_fff9, 0x8000_0000, 0x7fff_ffff];
    // SDIV/UDIV A32 r0 = r1 / r2, Thumb
    for &x in &vals {
        for &y in &vals {
            for w in [0xe710_f211u32, 0xe730_f211] {
                let mut c = case();
                c.arm = arm_at(0x1000, &[w, BKPT]);
                c.regs = vec![(15, 0x1000), (1, x), (2, y)];
                report(&mut fails, &format!("div {w:08x} {x:#x}/{y:#x}"), cmp(&c));
            }
            for (h1, h2) in [(0xfb91u16, 0xf0f2u16), (0xfbb1, 0xf0f2)] {
                let mut c = case();
                c.thumb = th_at(0x1000, &[h1, h2, 0xbe00]);
                c.regs = vec![(15, 0x1000), (1, x), (2, y)];
                c.cpsr |= 0x20;
                report(&mut fails, &format!("t div {h1:04x} {x:#x}/{y:#x}"), cmp(&c));
            }
        }
    }
    // CLZ
    for &x in &vals {
        let mut c = case();
        c.arm = arm_at(0x1000, &[0xe16f_0f11, BKPT]);
        c.regs = vec![(15, 0x1000), (1, x)];
        report(&mut fails, &format!("clz {x:#x}"), cmp(&c));
    }
    // STM with writeback and the base in the list (first, middle); LDM base in list without wb
    for (name, w) in [
        ("stmia r1!,{r0,r1,r2}", 0xe8a1_0007u32),
        ("stmia r1!,{r1,r2}", 0xe8a1_0006),
        ("stmdb r1!,{r0,r1}", 0xe921_0003),
        ("stmib r1!,{r1,r3}", 0xe9a1_000a),
        ("stmda r1!,{r0,r1,r2}", 0xe821_0007),
        ("ldmia r1,{r0,r1,r2}", 0xe891_0007),
        ("ldmdb r1,{r1,r4}", 0xe911_0012),
        ("stmia r1,{r0,r1,r2}", 0xe881_0007),
        ("str r1,[r1,#4]!", 0xe5a1_1004),
        ("str r1,[r1],#4", 0xe481_1004),
        ("strh r1,[r1,#2]!", 0xe1e1_10b2),
        ("strd r0,[r1,#8]!", 0xe1e1_00f8),
        ("strd r0,[r0,#8]!", 0xe1e0_00f8),
    ] {
        let mut c = case();
        c.arm = arm_at(0x1000, &[w, BKPT]);
        c.regs = vec![(15, 0x1000), (0, 0x11), (1, 0x9000), (2, 0x22), (3, 0x33), (4, 0x44)];
        c.data = (0..32).map(|i| (0x8f80 + 4 * i, 0x1000_0000 + i)).collect();
        report(&mut fails, name, cmp(&c));
    }
    // Thumb STM/PUSH with the base in the list
    for (name, h) in [("stmia r1!,{r0,r1}", 0xc103u16), ("stmia r1!,{r1,r2}", 0xc106)] {
        let mut c = case();
        c.thumb = th_at(0x1000, &[h, 0xbe00]);
        c.regs = vec![(15, 0x1000), (0, 0x11), (1, 0x9000), (2, 0x22)];
        c.cpsr |= 0x20;
        report(&mut fails, name, cmp(&c));
    }
    // LDM/LDR loading the PC: ARM/Thumb targets
    for target in [0x2000u32, 0x3001, 0x2002, 0x2003] {
        for (name, w) in [
            ("ldmia r0,{r1,pc}", 0xe890_8002u32),
            ("ldmia r0!,{r1,pc}", 0xe8b0_8002),
            ("ldr pc,[r0,#4]", 0xe590_f004),
            ("ldr pc,[r0],#4 (post)", 0xe490_f004),
            ("ldrne pc,[r0,#4]", 0x1590_f004),
            ("ldmeq r0,{r1,pc}", 0x0890_8002),
            ("mov pc,r5", 0xe1a0_f005),
            ("add pc,r5,#0", 0xe285_f000),
            ("bx r5", 0xe12f_ff15),
            ("blx r5", 0xe12f_ff35),
        ] {
            for nz in [0x4000_0000u32, 0] {
                let mut c = case();
                let mut code = arm_at(0x1000, &[w, 0xe3a0_7007, BKPT]);
                code.extend(arm_at(0x2000, &[0xe3a0_6001, BKPT]));
                c.arm = code;
                c.thumb = th_at(0x3000, &[0x2602, 0xbe00]);
                c.regs = vec![(15, 0x1000), (0, 0x9000), (5, target)];
                c.cpsr |= nz;
                c.data = vec![(0x9000, 0x55), (0x9004, target), (0x9008, target)];
                report(&mut fails, &format!("{name} -> {target:#x} z={}", nz != 0), cmp(&c));
            }
        }
        // Thumb: pop {pc}, ldr.w pc, add pc, mov pc, bx
        for (name, code) in [
            ("pop {r1,pc}", vec![0xbd02u16]),
            ("ldr.w pc,[r0,#4]", vec![0xf8d0, 0xf004]),
            ("ldr.w pc,[r0],#4", vec![0xf850, 0xfb04]),
            ("ldm.w r0,{r1,pc}", vec![0xe890, 0x8002]),
            ("mov pc,r5", vec![0x46af]),
            ("bx r5", vec![0x4728]),
            ("blx r5", vec![0x47a8]),
            ("add pc,r5", vec![0x44af]),
        ] {
            let mut c = case();
            let mut h = code.clone();
            h.extend([0x2707u16, 0xbe00]);
            c.thumb = th_at(0x1000, &h);
            c.thumb.extend(th_at(0x3000, &[0x2602, 0xbe00]));
            c.arm = arm_at(0x2000, &[0xe3a0_6001, BKPT]);
            c.regs = vec![(15, 0x1000), (0, 0x9000), (13, 0x9000), (5, if name.starts_with("add") { target.wrapping_sub(0x1004) } else { target })];
            c.cpsr |= 0x20;
            c.data = vec![(0x9000, 0x55), (0x9004, target), (0x9008, target)];
            report(&mut fails, &format!("T {name} -> {target:#x}"), cmp(&c));
        }
    }
    // MRS after flag-setting arithmetic
    for &x in &vals {
        let mut c = case();
        c.arm = arm_at(0x1000, &[0xe091_0002, 0xe10f_3000, 0xe0b1_0002, 0xe10f_4000, BKPT]);
        c.regs = vec![(15, 0x1000), (1, x), (2, 0x8000_0001)];
        report(&mut fails, &format!("mrs {x:#x}"), cmp(&c));
    }
    // Self-modifying: a loop rewrites the next instruction in its own block
    {
        let mut c = case();
        c.arm = arm_at(
            0x1000,
            &[
                0xe3a0_0000, // mov r0, #0
                0xe3a0_9004, // mov r9, #4
                0xe2800001,  // loop: add r0, r0, #1
                0xe586_5000, // str r5, [r6]
                0xe2800100,  // add r0, r0, #0 (patched into add r0,#0x10 after first)
                0xe259_9001, // subs r9, r9, #1
                0x1aff_fffa, // bne loop
                BKPT,
            ],
        );
        c.regs = vec![(15, 0x1000), (5, 0xe280_0010), (6, 0x1010)];
        report(&mut fails, "smc", cmp(&c));
    }
    // a 32-bit Thumb instruction straddling a page, reached in a region
    {
        let mut c = case();
        let mut h = vec![0x2000u16]; // movs r0,#0
        // pad with adds r0,#1 up to 0x1ffe
        while 0x1f00 + 2 * h.len() as u32 != 0x1ffe {
            h.push(0x3001);
        }
        h.push(0xf100); // add.w r0, r0, #1 straddling (0x1ffe, 0x2000)
        h.push(0x0001);
        h.push(0xbe00);
        c.thumb = th_at(0x1f00, &h);
        c.regs = vec![(15, 0x1f00)];
        c.cpsr |= 0x20;
        report(&mut fails, "straddle", cmp(&c));
    }
    // IT with firstcond AL and an else slot (unpredictable): ITE AL; movs r0,#1; movs r1,#2
    {
        let mut c = case();
        c.thumb = th_at(0x1000, &[0xbfec, 0x2001, 0x2102, 0xbe00]);
        c.regs = vec![(15, 0x1000)];
        c.cpsr |= 0x20;
        report(&mut fails, "ite al", cmp(&c));
    }
    // LDM crossing the end of RAM: the fault must leave the same state
    for w in [0xe8b0_001eu32, 0xe890_001e, 0xe590_1000, 0xe5b0_1004, 0xe1c0_20d0] {
        let mut c = case();
        c.arm = arm_at(0x1000, &[0xe3a0_7001, w, 0xe3a0_7002, BKPT]);
        c.regs = vec![(15, 0x1000), (0, RAM as u32 - 8)];
        report(&mut fails, &format!("fault {w:08x}"), cmp(&c));
    }
    // chain_until small in a loop: instret must match
    for chain in [0u64, 1, 2, 3, 5, 9, 17] {
        let mut c = case();
        c.arm = arm_at(0x1000, &[0xe3a0_9010, 0xe280_0001, 0xe259_9001, 0x1aff_fffc, BKPT]);
        c.regs = vec![(15, 0x1000)];
        c.chain = chain;
        report(&mut fails, &format!("chain {chain}"), cmp(&c));
    }
    for f in &fails {
        eprintln!("FAIL {f}");
    }
    assert!(fails.is_empty(), "{} failures", fails.len());
}

/// Random cases (as `jit_diff.rs`) with chaining budgets from none to unlimited.
#[test]
fn random_cases_with_chaining_budgets() {
    if !jit_ready("random_cases_with_chaining_budgets") {
        return;
    }
    let seed: u64 = 12345;
    let n: u32 = std::env::var("JIT_DIFF_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(1000);
    let mut rng = Rng(seed | 1);
    let mut fails = 0;
    for i in 0..n {
        let chain = [u64::MAX, 0, 1, 7, 40, 100][(i % 6) as usize];
        for kind in 0..4 {
            let (thumb, arm, code16, s) = match kind {
                0 => { let (c, s) = arm_case(&mut rng); (false, c, vec![], s) }
                1 => { let (c, s) = thumb_case(&mut rng); (true, vec![], c, s) }
                2 => { let (c, s) = arm_loop_case(&mut rng); (false, c, vec![], s) }
                _ => { let (c, s) = thumb_loop_case(&mut rng); (true, vec![], c, s) }
            };
            if let Err(e) = compare_case(thumb, &arm, &code16, s, chain) {
                if e.starts_with("interpreter:") { continue; }
                fails += 1;
                let dis: Vec<String> = if thumb { code16.iter().map(|h| format!("{h:04x}")).collect() } else { arm.iter().map(|w| format!("{w:08x}")).collect() };
                if fails < 20 {
                    eprintln!("FAIL seed {seed} i {i} kind {kind} chain {chain} s {s}: {e}; code {}", dis.join(" "));
                }
            }
        }
    }
    assert_eq!(fails, 0);
}

fn compare_mode(thumb: bool, arm: &[u32], code16: &[u16], seed: u64, chain_until: u64, mode: u32) -> Result<(), String> {
    let mk = || {
        let (mut c, r) = if thumb { setup_thumb(&mut Rng(seed), code16) } else { setup(&mut Rng(seed), arm) };
        c.cpsr = (c.cpsr & !0x1f) | mode;
        (c, r)
    };
    let (mut a, mut ram_a) = mk();
    let (mut b, mut ram_b) = mk();
    try_run(&mut a, &mut ram_a).map_err(|e| format!("interpreter: {e}"))?;
    // SAFETY: ram_b outlives b
    unsafe { b.set_ram(ram_b.mem.as_mut_ptr(), 0, RAM as u32) };
    if !b.enable_jit(CodeWatch::new(0, RAM as u32)) {
        return Err("no JIT".into());
    }
    b.jit_set_hot(1);
    b.chain_until = chain_until;
    let rb = try_run(&mut b, &mut ram_b);
    if rb.is_err() { return Err(format!("compiled: {rb:?}")); }
    if a.r != b.r { return Err(format!("registers: interpreter {:08x?}, compiled {:08x?}", a.r, b.r)); }
    if a.cpsr != b.cpsr { return Err(format!("cpsr: {:08x} vs {:08x}", a.cpsr, b.cpsr)); }
    if a.instret() != b.instret() { return Err(format!("instret: {} vs {}", a.instret(), b.instret())); }
    if ram_a.mem != ram_b.mem { return Err("memory".into()); }
    Ok(())
}

#[test]
fn random_cases_in_user_system_and_irq_mode() {
    if !jit_ready("random_cases_in_user_system_and_irq_mode") {
        return;
    }
    let seed: u64 = 999;
    let n: u32 = std::env::var("JIT_DIFF_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(500);
    let mut rng = Rng(seed | 1);
    let mut fails = 0;
    for i in 0..n {
        let chain = [u64::MAX, 0, 1, 7, 40, 100][(i % 6) as usize];
        for kind in 0..4 {
            let (thumb, arm, code16, s) = match kind {
                0 => { let (c, s) = arm_case(&mut rng); (false, c, vec![], s) }
                1 => { let (c, s) = thumb_case(&mut rng); (true, vec![], c, s) }
                2 => { let (c, s) = arm_loop_case(&mut rng); (false, c, vec![], s) }
                _ => { let (c, s) = thumb_loop_case(&mut rng); (true, vec![], c, s) }
            };
            for mode in [0x10u32, 0x1f, 0x12] {
                if let Err(e) = compare_mode(thumb, &arm, &code16, s, chain, mode) {
                    if e.starts_with("interpreter:") { continue; }
                    fails += 1;
                    if fails < 20 { eprintln!("FAIL mode {mode:x} i {i} kind {kind} chain {chain}: {e}"); }
                }
            }
        }
    }
    assert_eq!(fails, 0);
}

/// Interworking across pages (the native dispatcher) in user mode.
#[test]
fn interworking_across_pages_in_every_mode() {
    if !jit_ready("interworking_across_pages_in_every_mode") {
        return;
    }
    let mut fails = Vec::new();
    for mode in [0x10u32, 0x13, 0x1f] {
        for chain in [u64::MAX, 3, 10, 50] {
            let mut c = case();
            c.thumb = th_at(0x1000, &[0x2405, 0x4798, 0x3c01, 0xd1fc, 0xbe00]);
            c.arm = arm_at(0x2000, &[0xe92d_4010, 0xe280_0001, 0xfa00_03fc, 0xe8bd_4010, 0xe12f_ff1e]);
            c.thumb.extend(th_at(0x3000, &[0xb500, 0x3102, 0xbd00]));
            c.regs = vec![(15, 0x1000), (3, 0x2000), (13, 0x9000)];
            c.cpsr = 0x20 | mode;
            c.chain = chain;
            report(&mut fails, &format!("mode {mode:x} chain {chain}"), cmp(&c));
        }
    }
    for f in &fails { eprintln!("FAIL {f}"); }
    assert!(fails.is_empty());
}

#[test]
fn thumb_control_flow_and_literals() {
    if !jit_ready("thumb_control_flow_and_literals") {
        return;
    }
    let mut fails = Vec::new();
    // CBZ/CBNZ r0 forward over a movs r1,#1 (offset 0 -> target pc+4)
    for v in [0u32, 1, 0x8000_0000] {
        for (name, h) in [("cbz", 0xb100u16), ("cbnz", 0xb900)] {
            let mut c = case();
            c.thumb = th_at(0x1000, &[h, 0x2101, 0x2202, 0xbe00]);
            c.regs = vec![(15, 0x1000), (0, v)];
            c.cpsr |= 0x20;
            report(&mut fails, &format!("{name} {v:#x}"), cmp(&c));
        }
    }
    // ADD/SUB SP, #imm; ADR at pc%4 == 2 and 0; LDR literal at both alignments
    for pad in [0usize, 1] {
        let mut h = vec![0xbf00u16; pad];
        h.extend([0xb082, 0xb005, 0xa001, 0x4901, 0x4a02, 0xbe00, 0, 0x1234, 0x5678, 0x9abc, 0xdef0, 0x1111]);
        let mut c = case();
        c.thumb = th_at(0x1000, &h);
        c.regs = vec![(15, 0x1000), (13, 0x9000)];
        c.cpsr |= 0x20;
        report(&mut fails, &format!("sp/adr/ldr lit pad {pad}"), cmp(&c));
    }
    // T32 B<cond>.W over a movs, every cond under every NZCV
    for cond in 0..14u32 {
        for nzcv in 0..16u32 {
            let mut c = case();
            // b<cond>.w +2 (skip the movs r0,#1): imm = 2 -> hw2 imm11 = 1
            c.thumb = th_at(0x1000, &[(0xf000 | cond << 6) as u16, 0x8001, 0x2001, 0x2102, 0xbe00]);
            c.regs = vec![(15, 0x1000)];
            c.cpsr = 0x1d3 | 0x20 | nzcv << 28;
            report(&mut fails, &format!("b{cond}.w nzcv {nzcv:04b}"), cmp(&c));
        }
    }
    // T32 BL to a Thumb routine on the same page, BLX imm to an ARM routine on another page
    {
        let mut c = case();
        // 0x1000: bl 0x1010 ; movs r2,#2 ; bkpt ; ... 0x1010: movs r1,#1 ; bx lr
        let mut h = vec![0xf000u16, 0xf806, 0x2202, 0xbe00, 0xbf00, 0xbf00, 0xbf00, 0xbf00];
        h.extend([0x2101, 0x4770]);
        c.thumb = th_at(0x1000, &h);
        c.regs = vec![(15, 0x1000)];
        c.cpsr |= 0x20;
        report(&mut fails, "bl.w", cmp(&c));
        let mut c = case();
        // blx 0x2000 from 0x1000: offset = 0x2000 - 0x1004 = 0xffc
        let off = 0x2000u32 - 0x1004;
        let (s, i1, i2) = (0u32, 0u32, 0u32);
        let (j1, j2) = ((!i1 ^ s) & 1, (!i2 ^ s) & 1);
        let hw1 = 0xf000 | s << 10 | (off >> 12) & 0x3ff;
        let hw2 = 0xc000 | j1 << 13 | j2 << 11 | (off >> 1) & 0x7ff;
        c.thumb = th_at(0x1000, &[hw1 as u16, hw2 as u16, 0x2202, 0xbe00]);
        c.arm = arm_at(0x2000, &[0xe3a0_1001, 0xe12f_ff1e]);
        c.regs = vec![(15, 0x1000)];
        c.cpsr |= 0x20;
        report(&mut fails, "blx imm", cmp(&c));
    }
    // MRC TPIDRURW/TPIDRURO/TPIDRPRW, A32 and T32
    for opc2 in 2..=4u32 {
        let w = 0xee1d_0f10 | opc2 << 5;
        let mut c = case();
        c.arm = arm_at(0x1000, &[0xe3a0_0001, w, BKPT]);
        c.regs = vec![(15, 0x1000)];
        report(&mut fails, &format!("mrc {opc2}"), cmp(&c));
        let mut c = case();
        c.thumb = th_at(0x1000, &[(w >> 16) as u16, w as u16, 0xbe00]);
        c.regs = vec![(15, 0x1000)];
        c.cpsr |= 0x20;
        report(&mut fails, &format!("t mrc {opc2}"), cmp(&c));
    }
    // A32 BL into the same page and back, in a loop
    {
        let mut c = case();
        c.arm = arm_at(0x1000, &[0xe3a0_9005, 0xeb00_0003, 0xe259_9001, 0x1aff_fffc, BKPT, 0, 0xe280_0003, 0xe12f_ff1e]);
        c.regs = vec![(15, 0x1000)];
        report(&mut fails, "bl loop", cmp(&c));
    }
    // A32 code running up to a page end
    {
        let mut c = case();
        let mut w = vec![0xe280_0001u32; 0x40];
        w.push(BKPT);
        c.arm = arm_at(0x1f80, &w);
        c.regs = vec![(15, 0x1f80)];
        report(&mut fails, "page end", cmp(&c));
    }
    for f in &fails { eprintln!("FAIL {f}"); }
    assert!(fails.is_empty(), "{} failures", fails.len());
}

#[test]
fn t32_signed_word_loads_are_undefined() {
    if !jit_ready("t32_signed_word_loads_are_undefined") {
        return;
    }
    let mut fails = Vec::new();
    for (h1, h2) in [(0xf9d0u16, 0x1004u16), (0xf950, 0x1c04), (0xf950, 0x1004)] {
        let mut c = case();
        c.thumb = th_at(0x1000, &[0x2201, h1, h2, 0x2302, 0xbe00]);
        c.regs = vec![(15, 0x1000), (0, 0x9000)];
        c.data = vec![(0x9000, 0xaaaa_aaaa), (0x9004, 0x1234_5678)];
        c.cpsr |= 0x20;
        report(&mut fails, &format!("{h1:04x} {h2:04x}"), cmp(&c));
    }
    for f in &fails { eprintln!("FAIL {f}"); }
    assert!(fails.is_empty());
}
