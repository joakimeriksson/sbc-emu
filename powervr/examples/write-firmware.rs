//! Write the stand-in PowerVR firmware file: `cargo run -p powervr --example write-firmware OUT`
fn main() {
    let out = std::env::args().nth(1).expect("usage: write-firmware OUT");
    std::fs::write(&out, powervr::firmware::stand_in_image()).expect("write");
}
