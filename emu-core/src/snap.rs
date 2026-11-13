//! Tiny versioned binary serializer for machine snapshots (little-endian, no dependencies).
//! Each component writes its fields in a fixed order and reads them back in the same order;
//! a tag string before each component catches mismatched layouts.

#[derive(Default)]
pub struct W(pub Vec<u8>);

impl W {
    pub fn u8(&mut self, v: u8) { self.0.push(v); }
    pub fn bool(&mut self, v: bool) { self.0.push(v as u8); }
    pub fn u16(&mut self, v: u16) { self.0.extend_from_slice(&v.to_le_bytes()); }
    pub fn u32(&mut self, v: u32) { self.0.extend_from_slice(&v.to_le_bytes()); }
    pub fn u64(&mut self, v: u64) { self.0.extend_from_slice(&v.to_le_bytes()); }
    pub fn u128(&mut self, v: u128) { self.0.extend_from_slice(&v.to_le_bytes()); }
    pub fn bytes(&mut self, v: &[u8]) { self.u64(v.len() as u64); self.0.extend_from_slice(v); }
    pub fn tag(&mut self, t: &str) { self.bytes(t.as_bytes()); }
    pub fn opt_u64(&mut self, v: Option<u64>) { self.bool(v.is_some()); self.u64(v.unwrap_or(0)); }
    pub fn opt_u32(&mut self, v: Option<u32>) { self.bool(v.is_some()); self.u32(v.unwrap_or(0)); }
}

pub struct R<'a> { pub b: &'a [u8], pub p: usize }

#[derive(Debug)]
pub struct SnapError(pub String);

impl<'a> R<'a> {
    pub fn new(b: &'a [u8]) -> Self { R { b, p: 0 } }
    fn take(&mut self, n: usize) -> Result<&'a [u8], SnapError> {
        let s = self.b.get(self.p..self.p + n).ok_or_else(|| SnapError(format!("truncated snapshot at {}", self.p)))?;
        self.p += n;
        Ok(s)
    }
    pub fn u8(&mut self) -> Result<u8, SnapError> { Ok(self.take(1)?[0]) }
    pub fn bool(&mut self) -> Result<bool, SnapError> { Ok(self.u8()? != 0) }
    pub fn u16(&mut self) -> Result<u16, SnapError> { Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap())) }
    pub fn u32(&mut self) -> Result<u32, SnapError> { Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap())) }
    pub fn u64(&mut self) -> Result<u64, SnapError> { Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap())) }
    pub fn u128(&mut self) -> Result<u128, SnapError> { Ok(u128::from_le_bytes(self.take(16)?.try_into().unwrap())) }
    pub fn bytes(&mut self) -> Result<&'a [u8], SnapError> { let n = self.u64()? as usize; self.take(n) }
    pub fn tag(&mut self, t: &str) -> Result<(), SnapError> {
        let got = self.bytes()?;
        if got != t.as_bytes() { return Err(SnapError(format!("snapshot layout mismatch: expected '{t}', found '{}'", String::from_utf8_lossy(got)))); }
        Ok(())
    }
    pub fn opt_u64(&mut self) -> Result<Option<u64>, SnapError> { let s = self.bool()?; let v = self.u64()?; Ok(s.then_some(v)) }
    pub fn opt_u32(&mut self) -> Result<Option<u32>, SnapError> { let s = self.bool()?; let v = self.u32()?; Ok(s.then_some(v)) }
}
