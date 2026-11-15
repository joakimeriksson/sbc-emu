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

/// A parsed device tree, for boot loaders that adjust a distribution's DTB the way
/// U-Boot does (`/chosen`, `/memory`, firmware nodes) before handing it to the kernel.
/// Properties are kept as raw bytes, so phandles and unknown bindings survive unchanged.
/// The memory reservation block is not preserved.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Node {
    pub name: String,
    pub props: Vec<(String, Vec<u8>)>,
    pub children: Vec<Node>,
}

impl Node {
    pub fn new(name: &str) -> Self {
        Self { name: name.into(), ..Self::default() }
    }

    /// Parse a flattened device tree blob (version 16 or 17).
    pub fn parse(blob: &[u8]) -> Result<Self, String> {
        let word = |off: usize| -> Result<u32, String> {
            blob.get(off..off + 4)
                .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
                .ok_or_else(|| format!("device tree truncated at {off:#x}"))
        };
        if word(0)? != 0xd00d_feed {
            return Err("not a flattened device tree (bad magic)".into());
        }
        let total = word(4)? as usize;
        if total > blob.len() {
            return Err("device tree header size exceeds the file".into());
        }
        let (st_off, str_off) = (word(8)? as usize, word(12)? as usize);
        let string = |off: usize| -> Result<String, String> {
            let start = str_off + off;
            let rest = blob.get(start..total).ok_or("string offset out of range")?;
            let end = rest.iter().position(|&c| c == 0).ok_or("unterminated string")?;
            Ok(String::from_utf8_lossy(&rest[..end]).into_owned())
        };
        let mut stack: Vec<Node> = Vec::new();
        let mut root = None;
        let mut pos = st_off;
        loop {
            let token = word(pos)?;
            pos += 4;
            match token {
                BEGIN_NODE => {
                    let rest = blob.get(pos..total).ok_or("node name out of range")?;
                    let end = rest.iter().position(|&c| c == 0).ok_or("unterminated node name")?;
                    stack.push(Node::new(&String::from_utf8_lossy(&rest[..end])));
                    pos = (pos + end + 1 + 3) & !3;
                }
                END_NODE => {
                    let node = stack.pop().ok_or("unbalanced END_NODE")?;
                    match stack.last_mut() {
                        Some(parent) => parent.children.push(node),
                        None => root = Some(node),
                    }
                }
                PROP => {
                    let (len, name_off) = (word(pos)? as usize, word(pos + 4)? as usize);
                    pos += 8;
                    let value = blob.get(pos..pos + len).ok_or("property out of range")?.to_vec();
                    pos = (pos + len + 3) & !3;
                    stack.last_mut().ok_or("property outside a node")?.props.push((string(name_off)?, value));
                }
                4 => {} // NOP
                END => break,
                other => return Err(format!("bad device tree token {other:#x} at {:#x}", pos - 4)),
            }
        }
        root.ok_or_else(|| "device tree has no root node".into())
    }

    /// Serialize to a version-17 blob.
    pub fn to_blob(&self) -> Vec<u8> {
        fn emit(node: &Node, fdt: &mut Fdt) {
            fdt.begin(&node.name);
            for (name, value) in &node.props {
                fdt.prop(name, value);
            }
            for child in &node.children {
                emit(child, fdt);
            }
            fdt.end();
        }
        let mut fdt = Fdt::default();
        emit(self, &mut fdt);
        fdt.finish()
    }

    pub fn prop(&self, name: &str) -> Option<&[u8]> {
        self.props.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_slice())
    }

    /// Replace or add a property.
    pub fn set(&mut self, name: &str, value: impl Into<Vec<u8>>) {
        let value = value.into();
        match self.props.iter_mut().find(|(n, _)| n == name) {
            Some(slot) => slot.1 = value,
            None => self.props.push((name.into(), value)),
        }
    }

    pub fn set_str(&mut self, name: &str, value: &str) {
        let mut bytes = value.as_bytes().to_vec();
        bytes.push(0);
        self.set(name, bytes);
    }

    pub fn set_cells(&mut self, name: &str, cells: &[u32]) {
        self.set(name, cells.iter().flat_map(|c| c.to_be_bytes()).collect::<Vec<u8>>());
    }

    /// The child whose name is `name` or starts with `name@`, created if absent.
    pub fn child_mut(&mut self, name: &str) -> &mut Node {
        let matches = |n: &Node| n.name == name || n.name.split('@').next() == Some(name) && !name.contains('@');
        match self.children.iter().position(matches) {
            Some(index) => &mut self.children[index],
            None => {
                self.children.push(Node::new(name));
                self.children.last_mut().unwrap()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_round_trips_and_edits() {
        let mut fdt = Fdt::default();
        fdt.begin("");
        fdt.cells("#address-cells", &[1]);
        fdt.begin("chosen");
        fdt.str("bootargs", "old");
        fdt.end();
        fdt.begin("memory@40000000");
        fdt.cells("reg", &[0x4000_0000, 0x2000_0000]);
        fdt.end();
        fdt.end();
        let blob = fdt.finish();
        let mut root = Node::parse(&blob).unwrap();
        assert_eq!(Node::parse(&root.to_blob()).unwrap(), root);
        root.child_mut("chosen").set_str("bootargs", "console=ttyS0");
        root.child_mut("memory").set_cells("reg", &[0x4000_0000, 0x4000_0000]);
        root.child_mut("psci").set_str("method", "smc");
        let edited = Node::parse(&root.to_blob()).unwrap();
        assert_eq!(edited.children.len(), 3);
        assert_eq!(edited.children[0].prop("bootargs"), Some(&b"console=ttyS0\0"[..]));
        assert_eq!(edited.children[1].prop("reg").unwrap()[4..8], [0x40, 0, 0, 0]);
    }
}
