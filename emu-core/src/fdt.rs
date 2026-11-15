//! Minimal flattened device tree writer shared by board boot loaders.
#[derive(Default)]
pub struct Fdt { st: Vec<u8>, strings: Vec<u8> }

const BEGIN_NODE: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const END: u32 = 9;

impl Fdt {
    fn u32(&mut self, v: u32) { self.st.extend_from_slice(&v.to_be_bytes()); }
    fn pad(&mut self) { while self.st.len() % 4 != 0 { self.st.push(0); } }
    pub fn begin(&mut self, name: &str) { self.u32(BEGIN_NODE); self.st.extend_from_slice(name.as_bytes()); self.st.push(0); self.pad(); }
    pub fn end(&mut self) { self.u32(END_NODE); }
    pub fn prop(&mut self, name: &str, val: &[u8]) {
        let off = match find(&self.strings, name) {
            Some(o) => o,
            None => { let o = self.strings.len(); self.strings.extend_from_slice(name.as_bytes()); self.strings.push(0); o }
        };
        self.u32(PROP);
        self.u32(val.len() as u32);
        self.u32(off as u32);
        self.st.extend_from_slice(val);
        self.pad();
    }
    pub fn cells(&mut self, name: &str, v: &[u32]) {
        let b: Vec<u8> = v.iter().flat_map(|c| c.to_be_bytes()).collect();
        self.prop(name, &b);
    }
    pub fn str(&mut self, name: &str, v: &str) { self.strs(name, &[v]); }
    pub fn strs(&mut self, name: &str, v: &[&str]) {
        let mut b = Vec::new();
        for s in v { b.extend_from_slice(s.as_bytes()); b.push(0); }
        self.prop(name, &b);
    }
    pub fn empty(&mut self, name: &str) { self.prop(name, &[]); }

    pub fn finish(mut self) -> Vec<u8> {
        self.u32(END);
        let rsv_off = 40u32;
        let st_off = rsv_off + 16;
        let str_off = st_off + self.st.len() as u32;
        let total = str_off + self.strings.len() as u32;
        let mut out = Vec::with_capacity(total as usize);
        for v in [0xd00d_feedu32, total, st_off, str_off, rsv_off, 17, 16, 0, self.strings.len() as u32, self.st.len() as u32] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.extend_from_slice(&[0; 16]);
        out.extend_from_slice(&self.st);
        out.extend_from_slice(&self.strings);
        out
    }
}

fn find(strings: &[u8], name: &str) -> Option<usize> {
    let mut i = 0;
    while i < strings.len() {
        let end = i + strings[i..].iter().position(|&c| c == 0)?;
        if &strings[i..end] == name.as_bytes() { return Some(i); }
        i = end + 1;
    }
    None
}
