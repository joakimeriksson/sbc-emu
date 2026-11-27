//! Prints the decoder's form of a dumped SM70 program (SBC_DUMP_SHADERS writes them):
//!   cargo run -p maxwell-shader --example sm70dis -- FILE.bin [ENTRY_HEX]
fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: sm70dis FILE.bin [ENTRY_HEX]");
    let entry = args.next().map_or(0, |e| u32::from_str_radix(e.trim_start_matches("0x"), 16).expect("entry in hex"));
    let bytes = std::fs::read(&path).expect("read the dump");
    let mut fetch = |at: u32| bytes.get(at as usize..at as usize + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()));
    match maxwell_shader::program::Program::decode_sm70_at(entry, &mut fetch) {
        Ok(p) => print!("{p}"),
        Err(e) => eprintln!("{e}"),
    }
}
