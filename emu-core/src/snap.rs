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
        let s = self.p.checked_add(n).and_then(|e| self.b.get(self.p..e)).ok_or_else(|| SnapError(format!("truncated snapshot at {}", self.p)))?;
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

// ---------------------------------------------------------------------- whole structures

/// State that goes into a snapshot. `load` works in place: what is not part of the snapshot
/// (host resources, configuration) stays as it is.
pub trait Snap {
    fn save(&self, w: &mut W);
    fn load(&mut self, r: &mut R) -> Result<(), SnapError>;
}

/// Implement `Snap` for a struct by naming **every** field: a field that is in none of the lists
/// does not compile, so new state cannot be forgotten.
///
/// `{ saved }`, then optionally `fixed { .. }` (containers whose length is the hardware's:
/// another length in the snapshot is an error), `fitted { part: "what it is" }` (an `Option` the
/// machine is built with or without: the same in both), `skip { .. }` (configuration, host resources,
/// caches), and `after |s| ..` (runs after a load: drop caches, check ranges).
#[macro_export]
macro_rules! snap_struct {
    ($ty:ty { $($f:ident),* $(,)? } $(fixed { $($x:ident),* $(,)? })? $(fitted { $($o:ident : $what:expr),* $(,)? })? $(skip { $($s:ident),* $(,)? })? $(after $h:expr)?) => {
        impl $crate::snap::Snap for $ty {
            fn save(&self, w: &mut $crate::snap::W) {
                let Self { $($f,)* $($($x,)*)? $($($o,)*)? $($($s: _,)*)? } = self;
                $($crate::snap::Snap::save($f, w);)*
                $($($crate::snap::Snap::save($x, w);)*)?
                $($($crate::snap::save_fitted(w, $o);)*)?
                let _ = w;
            }
            fn load(&mut self, r: &mut $crate::snap::R) -> Result<(), $crate::snap::SnapError> {
                let Self { $($f,)* $($($x,)*)? $($($o,)*)? $($($s: _,)*)? } = self;
                $($crate::snap::Snap::load($f, r)?;)*
                $($(
                    let n = $x.len();
                    $crate::snap::Snap::load($x, r)?;
                    if $x.len() != n {
                        return Err($crate::snap::SnapError(format!("{}.{}: {} entries in the snapshot, {n} here", stringify!($ty), stringify!($x), $x.len())));
                    }
                )*)?
                $($($crate::snap::load_fitted(r, $o, $what)?;)*)?
                let _ = r;
                $(
                    let after: fn(&mut Self) -> Result<(), $crate::snap::SnapError> = $h;
                    after(self)?;
                )?
                Ok(())
            }
        }
    };
}

/// Implement `Snap` for an enum without data by naming every variant (in a fixed order: it is
/// the format).
#[macro_export]
macro_rules! snap_enum {
    ($ty:ty { $($v:ident),* $(,)? }) => {
        impl $crate::snap::Snap for $ty {
            fn save(&self, w: &mut $crate::snap::W) {
                let mut n = 0u8;
                $(if matches!(self, <$ty>::$v) { return w.u8(n); } n += 1;)*
                let _ = n;
                match self { $(<$ty>::$v => {})* }
            }
            fn load(&mut self, r: &mut $crate::snap::R) -> Result<(), $crate::snap::SnapError> {
                let got = r.u8()?;
                let mut n = 0u8;
                $(if got == n { *self = <$ty>::$v; return Ok(()); } n += 1;)*
                Err($crate::snap::SnapError(format!("{}: no variant {got} of {n}", stringify!($ty))))
            }
        }
    };
}

macro_rules! snap_int {
    ($($t:ty),*) => { $(
        impl Snap for $t {
            fn save(&self, w: &mut W) { w.0.extend_from_slice(&self.to_le_bytes()); }
            fn load(&mut self, r: &mut R) -> Result<(), SnapError> {
                *self = <$t>::from_le_bytes(r.take(std::mem::size_of::<$t>())?.try_into().unwrap());
                Ok(())
            }
        }
    )* };
}
snap_int!(u8, u16, u32, u64, u128, i8, i16, i32, i64, f32, f64);

impl Snap for usize {
    fn save(&self, w: &mut W) { w.u64(*self as u64); }
    fn load(&mut self, r: &mut R) -> Result<(), SnapError> {
        *self = usize::try_from(r.u64()?).map_err(|_| SnapError("a length does not fit this host".into()))?;
        Ok(())
    }
}

impl Snap for bool {
    fn save(&self, w: &mut W) { w.bool(*self); }
    fn load(&mut self, r: &mut R) -> Result<(), SnapError> { *self = r.bool()?; Ok(()) }
}

impl<T: Snap, const N: usize> Snap for [T; N] {
    fn save(&self, w: &mut W) { for v in self { v.save(w); } }
    fn load(&mut self, r: &mut R) -> Result<(), SnapError> { for v in self { v.load(r)?; } Ok(()) }
}

impl<T: Snap + Default> Snap for Option<T> {
    fn save(&self, w: &mut W) {
        w.bool(self.is_some());
        if let Some(v) = self { v.save(w); }
    }
    fn load(&mut self, r: &mut R) -> Result<(), SnapError> {
        if !r.bool()? { *self = None; return Ok(()); }
        self.get_or_insert_with(T::default).load(r)
    }
}

/// `n` values; the memory they take grows as they are read, whatever `n` claims.
fn load_n<T: Snap + Default>(r: &mut R, mut push: impl FnMut(T)) -> Result<(), SnapError> {
    for _ in 0..r.u64()? {
        let mut v = T::default();
        v.load(r)?;
        push(v);
    }
    Ok(())
}

macro_rules! snap_sequence {
    ($($c:ident, $push:ident $(, $bound:ident)?);*) => { $(
        impl<T: Snap + Default $(+ $bound)?> Snap for std::collections::$c<T> {
            fn save(&self, w: &mut W) {
                w.u64(self.len() as u64);
                for v in self { v.save(w); }
            }
            fn load(&mut self, r: &mut R) -> Result<(), SnapError> {
                self.clear();
                load_n(r, |v| { self.$push(v); })
            }
        }
    )* };
}
snap_sequence!(VecDeque, push_back; BTreeSet, insert, Ord);

impl<T: Snap + Default> Snap for Vec<T> {
    fn save(&self, w: &mut W) {
        w.u64(self.len() as u64);
        for v in self { v.save(w); }
    }
    fn load(&mut self, r: &mut R) -> Result<(), SnapError> {
        self.clear();
        load_n(r, |v| self.push(v))
    }
}

impl<K: Snap + Default + Ord, V: Snap + Default> Snap for std::collections::BTreeMap<K, V> {
    fn save(&self, w: &mut W) {
        w.u64(self.len() as u64);
        for (k, v) in self { k.save(w); v.save(w); }
    }
    fn load(&mut self, r: &mut R) -> Result<(), SnapError> {
        self.clear();
        load_n::<(K, V)>(r, |(k, v)| { self.insert(k, v); })
    }
}

/// In the order of the keys: a snapshot does not depend on the host's hashing.
impl<K: Snap + Default + Ord + std::hash::Hash, V: Snap + Default> Snap for std::collections::HashMap<K, V> {
    fn save(&self, w: &mut W) {
        let mut entries: Vec<_> = self.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        w.u64(entries.len() as u64);
        for (k, v) in entries { k.save(w); v.save(w); }
    }
    fn load(&mut self, r: &mut R) -> Result<(), SnapError> {
        self.clear();
        load_n::<(K, V)>(r, |(k, v)| { self.insert(k, v); })
    }
}

impl Snap for crate::FlatRam {
    fn save(&self, w: &mut W) { save_ram(w, &self.mem); }
    fn load(&mut self, r: &mut R) -> Result<(), SnapError> { load_ram(r, &mut self.mem) }
}

impl<T: Snap + Copy> Snap for std::cell::Cell<T> {
    fn save(&self, w: &mut W) { self.get().save(w); }
    fn load(&mut self, r: &mut R) -> Result<(), SnapError> { self.get_mut().load(r) }
}

impl Snap for std::sync::Arc<[u8]> {
    fn save(&self, w: &mut W) { w.bytes(self); }
    fn load(&mut self, r: &mut R) -> Result<(), SnapError> { *self = r.bytes()?.into(); Ok(()) }
}

macro_rules! snap_tuple {
    ($($t:ident $n:tt),*) => {
        impl<$($t: Snap),*> Snap for ($($t,)*) {
            fn save(&self, w: &mut W) { $(self.$n.save(w);)* }
            fn load(&mut self, r: &mut R) -> Result<(), SnapError> { $(self.$n.load(r)?;)* Ok(()) }
        }
    };
}
snap_tuple!(A 0, B 1);
snap_tuple!(A 0, B 1, C 2);
snap_tuple!(A 0, B 1, C 2, D 3);

/// Parts whose number is the hardware's, saved and loaded in place.
pub fn save_each<T: Snap>(w: &mut W, parts: &[T]) {
    w.u64(parts.len() as u64);
    for p in parts { p.save(w); }
}

pub fn load_each<T: Snap>(r: &mut R, parts: &mut [T], what: &str) -> Result<(), SnapError> {
    let n = r.u64()?;
    if n != parts.len() as u64 { return Err(SnapError(format!("the snapshot has {n} {what}, this machine {}", parts.len()))); }
    for p in parts { p.load(r)?; }
    Ok(())
}

/// Something the machine is built with or without: the same in the snapshot and here.
pub fn save_fitted<T: Snap>(w: &mut W, part: &Option<T>) {
    w.bool(part.is_some());
    if let Some(p) = part { p.save(w); }
}

pub fn load_fitted<T: Snap>(r: &mut R, part: &mut Option<T>, what: &str) -> Result<(), SnapError> {
    match (r.bool()?, part) {
        (true, Some(p)) => p.load(r),
        (false, None) => Ok(()),
        (true, None) => Err(SnapError(format!("the snapshot's machine has {what}, this one does not"))),
        (false, Some(_)) => Err(SnapError(format!("this machine has {what}, the snapshot's does not"))),
    }
}

/// Guest RAM: the pages that are not all zero.
pub fn save_ram(w: &mut W, ram: &[u8]) {
    const ZERO: [u8; 4096] = [0; 4096];
    w.u64(ram.len() as u64);
    for (n, page) in ram.chunks(4096).enumerate() {
        if page != &ZERO[..page.len()] {
            w.u32(n as u32);
            w.0.extend_from_slice(page);
        }
    }
    w.u32(u32::MAX);
}

pub fn load_ram(r: &mut R, ram: &mut [u8]) -> Result<(), SnapError> {
    let len = r.u64()?;
    if len != ram.len() as u64 {
        return Err(SnapError(format!("the snapshot's machine has {} MiB of RAM, this one {} MiB (--ram-mb)", len >> 20, ram.len() >> 20)));
    }
    let mut next = 0usize;
    loop {
        let n = r.u32()?;
        let at = if n == u32::MAX { ram.len() } else { n as usize * 4096 };
        if at > ram.len() || at < next || (n != u32::MAX && at == ram.len()) {
            return Err(SnapError(format!("RAM page {n} is out of range or order")));
        }
        // only pages that hold something: the host need not back the rest
        for page in ram[next..at].chunks_mut(4096) {
            if page.iter().any(|&b| b != 0) { page.fill(0); }
        }
        if n == u32::MAX { return Ok(()); }
        let end = (at + 4096).min(ram.len());
        ram[at..end].copy_from_slice(r.take(end - at)?);
        next = end;
    }
}

/// A disk's written sectors (`Disk::overlay`). The image itself is not in the snapshot.
pub fn save_disk(w: &mut W, disk: Option<&dyn crate::disk::Disk>) {
    w.bool(disk.is_some());
    if let Some(d) = disk {
        w.u64(d.sectors());
        let overlay = d.overlay();
        w.u64(overlay.len() as u64);
        for (lba, data) in overlay { w.u64(lba); w.bytes(&data); }
    }
}

/// An error for a disk that writes through to its image: a snapshot could not bring it back.
pub fn check_disk(disk: Option<&dyn crate::disk::Disk>) -> Result<(), String> {
    if disk.is_some_and(|d| !d.snapshots_disk()) {
        return Err("--sd-persist writes into the image: a snapshot could not bring it back".into());
    }
    Ok(())
}

/// The disk must be there, with the image it had.
pub fn load_disk(r: &mut R, disk: Option<&mut dyn crate::disk::Disk>, what: &str) -> Result<(), SnapError> {
    let Some(d) = disk else {
        return if r.bool()? { Err(SnapError(format!("the snapshot's machine has {what}: attach the same image"))) } else { Ok(()) };
    };
    // the guest would go on with caches of sectors this image does not have, and write them into it
    if !d.snapshots_disk() { return Err(SnapError(format!("{what}: --sd-persist writes into the image: nothing is restored onto it"))); }
    if !r.bool()? { return Err(SnapError(format!("this machine has {what}, the snapshot's does not"))); }
    let sectors = r.u64()?;
    if sectors != d.sectors() {
        return Err(SnapError(format!("{what}: the snapshot's image has {sectors} sectors, this one {}", d.sectors())));
    }
    let mut overlay = Vec::new();
    for _ in 0..r.u64()? { overlay.push((r.u64()?, r.bytes()?.to_vec())); }
    d.restore_overlay(overlay);
    Ok(())
}

// ---------------------------------------------------------------------- files

const MAGIC: &[u8; 8] = b"SBCSNAP2";

/// A snapshot file: which board, in which version of its layout, the sizes, and the state (zlib: its
/// checksum tells a damaged file).
pub fn pack(board: &str, version: u32, body: &[u8]) -> Vec<u8> {
    let mut w = W(MAGIC.to_vec());
    w.tag(board);
    w.u32(version);
    let packed = miniz_oxide::deflate::compress_to_vec_zlib(body, 1);
    w.u64(body.len() as u64);
    w.bytes(&packed);
    w.0
}

/// The state in `file`, if it is `board`'s in this `version` and no more than `limit` bytes (see
/// `limit`).
pub fn unpack(board: &str, version: u32, limit: u64, file: &[u8]) -> Result<Vec<u8>, SnapError> {
    if file.starts_with(b"SBCSNAP1") { return Err(SnapError("this is a Raspberry Pi 4 snapshot".into())); }
    let Some(rest) = file.strip_prefix(MAGIC) else { return Err(SnapError("not an sbc-emu snapshot".into())) };
    let mut r = R::new(rest);
    let from = String::from_utf8_lossy(r.bytes()?).into_owned();
    if from != board { return Err(SnapError(format!("this is a snapshot of the board '{from}', not '{board}'"))); }
    let v = r.u32()?;
    if v != version { return Err(SnapError(format!("the snapshot has layout version {v}, this build has {version}"))); }
    let damaged = |what: String| SnapError(format!("the snapshot is damaged ({what})"));
    let len = r.u64()?;
    let packed = r.bytes()?;
    if r.p != rest.len() { return Err(damaged(format!("{} bytes too many", rest.len() - r.p))); }
    if len > limit { return Err(SnapError(format!("the snapshot's state is {} MiB, this machine's at most {}", len >> 20, limit >> 20))); }
    let body = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(packed, len as usize).map_err(|e| damaged(format!("{:?}", e.status)))?;
    if body.len() as u64 != len { return Err(damaged(format!("{} bytes of {len}", body.len()))); }
    Ok(body)
}

/// The most state a machine can have: its RAM, every sector of its disks written, and the rest.
pub fn limit(ram: usize, sectors: u64) -> u64 { ram as u64 * 2 + sectors * 1024 + (256 << 20) }

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet, VecDeque};

    #[derive(Clone, Copy, PartialEq, Debug, Default)]
    enum Phase { #[default] Idle, Busy, Done }
    snap_enum!(Phase { Idle, Busy, Done });

    #[derive(Default, PartialEq, Debug, Clone)]
    struct Dev { part: Option<u32>, a: u32, phase: Phase, queue: VecDeque<(u8, u64)>, map: BTreeMap<u16, Vec<u8>>, set: BTreeSet<(u32, u32)>, last: Option<[u16; 3]>, f: f32, regs: Vec<u32>, cache: u32, config: bool }
    snap_struct!(Dev { a, phase, queue, map, set, last, f } fixed { regs } fitted { part: "a part" } skip { cache, config } after |s| { s.cache = 0; Ok(()) });

    fn dev() -> Dev {
        Dev { part: Some(3), a: 7, phase: Phase::Done, queue: [(1, 2), (3, 4)].into(), map: [(5, vec![6, 7])].into(), set: [(8, 9)].into(), last: Some([1, 2, 3]), f: -0.5, regs: vec![1, 2, 3], cache: 99, config: true }
    }

    #[test]
    fn a_structure_comes_back() {
        let d = dev();
        let mut w = W::default();
        d.save(&mut w);
        let mut other = Dev { part: Some(0), regs: vec![9; 3], cache: 5, queue: [(9, 9)].into(), map: [(1, vec![])].into(), ..Default::default() };
        other.load(&mut R::new(&w.0)).unwrap();
        // what is skipped stays this machine's; the cache is dropped
        assert_eq!(other, Dev { cache: 0, config: false, ..d });
        let mut none = Dev { last: None, ..dev() };
        let mut w2 = W::default();
        none.save(&mut w2);
        none.last = Some([4; 3]);
        none.load(&mut R::new(&w2.0)).unwrap();
        assert_eq!(none.last, None);
    }

    #[test]
    fn a_damaged_structure_is_an_error() {
        let mut w = W::default();
        dev().save(&mut w);
        for n in 0..w.0.len() { assert!(dev().load(&mut R::new(&w.0[..n])).is_err(), "cut at {n}"); }
        assert!(Dev { part: None, ..dev() }.load(&mut R::new(&w.0)).unwrap_err().0.contains("a part"));
        let mut d = Dev { regs: vec![0; 4], ..Default::default() };
        assert!(d.load(&mut R::new(&w.0)).unwrap_err().0.contains("regs"));
        assert!(Phase::Idle.load(&mut R::new(&[3])).is_err());
        // a length beyond the data neither allocates for it nor overflows
        assert!(Vec::<u64>::new().load(&mut R::new(&[0xff; 24])).is_err());
        assert!(R::new(&[0xff; 24]).bytes().is_err());
    }

    #[test]
    fn hash_maps_are_ordered() {
        let (mut a, mut b) = (W::default(), W::default());
        (0..50u32).map(|k| (k, k as u64)).collect::<std::collections::HashMap<_, _>>().save(&mut a);
        (0..50u32).rev().map(|k| (k, k as u64)).collect::<std::collections::HashMap<_, _>>().save(&mut b);
        assert_eq!(a.0, b.0);
        let mut m = std::collections::HashMap::<u32, u64>::from([(99, 1)]);
        m.load(&mut R::new(&a.0)).unwrap();
        assert!(m.len() == 50 && m[&7] == 7);
    }

    #[test]
    fn ram_pages() {
        let mut ram = vec![0u8; 5 * 4096 + 100];
        ram[4096 + 5] = 1;
        let n = ram.len();
        ram[n - 1] = 2;
        let mut w = W::default();
        save_ram(&mut w, &ram);
        assert!(w.0.len() < 4096 + 100 + 64);
        let mut other = vec![0xaa; ram.len()];
        load_ram(&mut R::new(&w.0), &mut other).unwrap();
        assert!(other == ram);
        assert!(load_ram(&mut R::new(&w.0), &mut vec![0; 4096]).is_err());
        for cut in [8, 9, 20, w.0.len() - 1] { assert!(load_ram(&mut R::new(&w.0[..cut]), &mut other).is_err()); }
        // a page past the end
        let mut bad = W::default();
        bad.u64(8192); bad.u32(2); bad.0.extend([0; 4096]); bad.u32(u32::MAX);
        assert!(load_ram(&mut R::new(&bad.0), &mut [0; 8192]).is_err());
    }

    #[test]
    fn files_say_what_they_are() {
        let f = pack("board-a", 3, b"state");
        assert_eq!(unpack("board-a", 3, 100, &f).unwrap(), b"state");
        assert!(unpack("board-b", 3, 100, &f).unwrap_err().0.contains("board-a"));
        assert!(unpack("board-a", 4, 100, &f).unwrap_err().0.contains("version 3"));
        assert!(unpack("board-a", 3, 100, b"SBCSNAP1xxxx").unwrap_err().0.contains("Pi 4"));
        assert!(unpack("board-a", 3, 100, b"hello").is_err());
        assert!(unpack("board-a", 3, 4, &f).unwrap_err().0.contains("at most"));
    }

    #[test]
    fn a_damaged_file_is_refused() {
        let body: Vec<u8> = (0..600u32).flat_map(|k| (k / 7).to_le_bytes()).collect();
        let f = pack("b", 1, &body);
        for n in 0..f.len() { assert!(unpack("b", 1, 1 << 20, &f[..n]).is_err(), "cut at {n}"); }
        assert!(unpack("b", 1, 1 << 20, &[&f[..], b"x"].concat()).is_err());
        for bit in 0..f.len() * 8 {
            let mut g = f.clone();
            g[bit / 8] ^= 1 << (bit % 8);
            // (some bits of the zlib header mean nothing: then the state is the right one)
            if let Ok(b) = unpack("b", 1, 1 << 20, &g) { assert!(b == body, "bit {bit}"); }
        }
    }

    #[test]
    fn disks() {
        use crate::disk::{Disk, MemDisk};
        let d = MemDisk::new(4);
        let mut w = W::default();
        save_disk(&mut w, Some(&d));
        let mut same = MemDisk::new(4);
        load_disk(&mut R::new(&w.0), Some(&mut same), "SD card").unwrap();
        assert!(load_disk(&mut R::new(&w.0), None, "SD card").unwrap_err().0.contains("SD card"));
        assert!(load_disk(&mut R::new(&w.0), Some(&mut MemDisk::new(5)), "SD card").unwrap_err().0.contains("5"));
        let mut none = W::default();
        save_disk(&mut none, None);
        assert!(load_disk(&mut R::new(&none.0), Some(&mut same), "SD card").is_err());
        assert!(load_disk(&mut R::new(&none.0), None, "SD card").is_ok());
        let _: &dyn Disk = &same;
    }
}
