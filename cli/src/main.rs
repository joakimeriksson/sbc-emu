//! `sbc-emu` command line.
//!
//!   sbc-emu boards list
//!   sbc-emu mkinitramfs --busybox PATH -o OUT.cpio
//!   sbc-emu run [--board pi4b] --kernel Image [--initrd X.cpio] [--append ARGS] [--max-insns N]
mod cpio;
mod disk;
#[cfg(not(target_arch = "wasm32"))]
mod esp32s31;
mod gdb;
mod monitor;
mod vnc;
mod vnc_des;
mod wav;
mod pi4;
mod terminal;
mod orangepi;
mod orangepi_one;
mod tinkerboard;
mod orangepi_rv;
mod jetson_nano;
mod serial_board;
mod script;

use sbc_machine::Board;
use std::io::Write;
use std::process::ExitCode;

fn arg<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("boards") => {
            for board in Board::ALL { println!("{:<10} {}", board.id(), board.description()); }
            ExitCode::SUCCESS
        }
        Some("mkinitramfs") => {
            let (Some(bb), Some(out)) = (arg(&args, "--busybox"), arg(&args, "-o")) else { return usage() };
            let board = match Board::from_args(&args) { Ok(b) => b, Err(e) => {eprintln!("{e}");return ExitCode::from(2);} };
            if let Err(e) = board.require_implemented() { eprintln!("{e}"); return ExitCode::from(2); }
            let busybox = match std::fs::read(bb) { Ok(b) => b, Err(e) => { eprintln!("{bb}: {e}"); return ExitCode::FAILURE } };
            let image = if board == Board::OrangePi5 { cpio::orangepi_initramfs(&busybox) } else { cpio::busybox_initramfs(&busybox) };
            std::fs::write(out, image).unwrap();
            ExitCode::SUCCESS
        }
        Some("run") => run(&args),
        Some("sd") => sd(&args),
        _ => usage(),
    }
}

fn sd(args: &[String]) -> ExitCode {
    let (Some(cmd), Some(img)) = (args.get(1), args.get(2)) else { return usage() };
    let mut d = match disk::CowFile::open(img) { Ok(d) => d, Err(e) => { eprintln!("{img}: {e}"); return ExitCode::FAILURE } };
    let parts = bcm2711::fat::mbr_partitions(&mut d);
    let Some(&(_, lba, _)) = parts.iter().find(|p| matches!(p.0, 0x0b | 0x0c | 0x0e | 0x06 | 0x04)) else { eprintln!("no FAT partition"); return ExitCode::FAILURE };
    let mut fat = bcm2711::fat::Fat::open(&mut d, lba).expect("FAT");
    match cmd.as_str() {
        "ls" => {
            let path = args.get(3).map(String::as_str).unwrap_or("/");
            let cl = if path == "/" { 0 } else { match fat.find(path) { Some(e) => e.cluster, None => { eprintln!("{path}: not found"); return ExitCode::FAILURE } } };
            for e in fat.list_dir(cl) { println!("{:>10} {}{}", e.size, e.name, if e.dir { "/" } else { "" }); }
        }
        "cat" => {
            let Some(path) = args.get(3) else { return usage() };
            match fat.read_file(path) { Some(b) => { let _ = std::io::stdout().write_all(&b); } None => { eprintln!("{path}: not found"); return ExitCode::FAILURE } }
        }
        _ => return usage(),
    }
    ExitCode::SUCCESS
}

fn usage() -> ExitCode {
    eprintln!("usage:\n  sbc-emu boards list\n  sbc-emu sd ls|cat IMAGE [PATH]\n  sbc-emu mkinitramfs [--board pi4b|orangepi5] --busybox PATH -o OUT.cpio\n  sbc-emu run [--board pi4b] --kernel Image [--initrd FILE] [--append ARGS] [--max-insns N] [--timeout SECS] [--screenshot OUT.png] [--shell serial|fb] [--input serial|usb] [--script FILE] [--restore SNAPSHOT] [--net nat|local|none] [--no-jit] [--gpio-log] [--ram-mb N] [--no-display] [--smp N] [--gdb PORT [--gdb-wait]] [--serial1 FILE] [--hostfwd tcp::HOSTPORT-:GUESTPORT] [--cpu-crypto] [--vnc :N[,password=PW]] [--threads single|multi] [--seed N] [--usb-drive IMAGE] [--otg-keyboard] [--audio-out FILE.wav] [--camera] [--no-bluetooth] [--display kms|fb] [--hdmi-mode WxH] [--no-gpu] [--sd-persist]\n  sbc-emu run [--board pi4b] --sd IMAGE.img [same options]   (boots like the firmware: config.txt, cmdline.txt, kernel8.img)\n  sbc-emu run --board orangepi5|orangepi-one|tinkerboard|orangepi-rv [--kernel FILE --dtb FILE | --sd IMAGE] [--gdb PORT [--gdb-wait]] ...   (docs/BOARD.md)\n  sbc-emu run --board esp32s31 --rom ROM.elf --flash FLASH.bin [--gdb PORT [--gdb-wait]] ...");
    ExitCode::from(2)
}


fn run(args: &[String]) -> ExitCode {
    let board = match Board::from_args(args) {
        Ok(board) => board,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(2); }
    };
    if let Err(e) = board.require_implemented() {
        eprintln!("{e}");
        return ExitCode::from(2);
    }
    match board {
        Board::Pi4 => pi4::run(args),
        Board::OrangePi5 => orangepi::run(args),
        Board::OrangePiOne => orangepi_one::run(args),
        Board::TinkerBoard => tinkerboard::run(args),
        Board::OrangePiRv => orangepi_rv::run(args),
        Board::Esp32S31 => esp32s31::run(args),
        Board::JetsonNano => jetson_nano::run(args),
        Board::Pi3 => unreachable!("checked above"),
    }
}
