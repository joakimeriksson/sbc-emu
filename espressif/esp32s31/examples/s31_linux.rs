//! Boot Espressif's ESP32-S31 Linux flash image (esp-buildroot-external
//! `espressif_esp32s31_function_core_board_nor_defconfig` → `s31_full_flash.bin`) on the
//! ESP32-S31-Function-CoreBoard-1: 16 MiB W25Q128 NOR, 16 MiB in-package PSRAM, the real mask
//! ROM. The chain is the board's own: ROM → U-Boot SPL → OpenSBI → U-Boot → XIP Linux.
//!   cargo run -p esp32s31 --release --example s31_linux -- ROM.elf s31_full_flash.bin [insns] [script]
//! UART0 goes to stdout. `script` is an esp-soc script (e.g. `5 uart0 uname -a`).
//! S31_TRACE=1 prints each privilege/trap transition of hart 0.
use esp_soc::SocBus as _;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 { eprintln!("usage: s31_linux ROM.elf s31_full_flash.bin [max_insns] [script]"); std::process::exit(2); }
    let max: u64 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(u64::MAX);
    let flash = std::fs::read(&a[2]).expect("flash image");
    let mut m = esp32s31::machine([0x10, 0x20, 0x30, 0x40, 0x50, 0x60], 16 << 20);
    m.bus.write_flash(0, &flash).unwrap();
    m.bus.set_board_inputs(esp32s31::soc::BoardInputs::function_coreboard_1()).unwrap();
    m.bus.set_strap(0x10);
    m.console.mask = 2;
    if let Some(script) = a.get(4) { m.load_script(&std::fs::read_to_string(script).expect("script")).expect("script"); }
    let rom = std::fs::read(&a[1]).unwrap();
    m.bus.load_mask_rom(&rom).unwrap();
    m.boot_rom(); m.cores[0].x[2] = esp32s31::ROM_STACK;
    let trace = std::env::var("S31_TRACE").is_ok();
    let max_reboots: u32 = std::env::var("S31_REBOOTS").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
    let mut reboots = 0u32;
    let mut done = 0u64;
    let mut last = (m.cores[0].priv_mode, m.cores[0].satp);
    while done < max {
        let chunk = if trace { 1 } else { 1_000_000 };
        let stop = m.run(chunk); done += chunk;
        if trace {
            let c = &m.cores[0];
            if (c.priv_mode, c.satp) != last {
                eprintln!("[s31] priv {}->{} satp={:#x} pc={:#x} mcause={:#x} scause={:#x} insns={}", last.0, c.priv_mode, c.satp, c.pc, c.mcause, c.scause, c.insn_count);
                last = (c.priv_mode, c.satp);
            }
        }
        match stop {
            esp_soc::Stop::MaxInsns => {}
            esp_soc::Stop::SwReset => {
                eprintln!("\n[s31] SwReset cause={:#x} pc={:#x}", m.bus.reset_cause(), m.cores[0].pc);
                reboots += 1;
                if reboots > max_reboots { break; }
                m.reboot();
            }
            s => { eprintln!("\n[s31] stop {s:?} pc={:#x}", m.cores[0].pc); break; }
        }
    }
    let c = &m.cores[0];
    eprintln!("\n[s31] pc={:#x} priv={} satp={:#x} mepc={:#x} mcause={:#x} mtval={:#x} sepc={:#x} scause={:#x} stval={:#x} insns={} walks={} fault={:?}",
        c.pc, c.priv_mode, c.satp, c.mepc, c.mcause, c.mtval, c.sepc, c.scause, c.stval, c.insn_count, c.tlb.walks, m.bus.last_fault());
}
