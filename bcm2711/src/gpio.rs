//! BCM2711 GPIO controller (Linux `pinctrl-bcm2835`, "brcm,bcm2711-gpio"): 58 lines, function
//! select, set/clear/level, the six edge/level detect enables, event status (W1C) and the
//! BCM2711 pull-up/down registers. Bank interrupts: lines 0..27, 28..45, 46..57.
//!
//! The host side: outputs are recorded as timestamped events (an LED on GPIO17 is the plan's
//! first board-peripheral proof); inputs (a button) are driven by `set_input`.

pub const NPINS: usize = 58;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PinEvent { pub pin: u8, pub level: bool, pub time_ns: u64 }

pub struct Gpio {
    fsel: [u8; NPINS],
    out: u64,
    /// externally driven input levels (None = floating: the pull decides)
    input: [Option<bool>; NPINS],
    /// 0 none, 1 pull-up, 2 pull-down (BCM2711 encoding)
    pull: [u8; NPINS],
    eds: u64,
    ren: u64, fen: u64, hen: u64, len: u64, aren: u64, afen: u64,
    last_level: u64,
    /// output changes for the host (drained by the machine/UI)
    pub events: Vec<PinEvent>,
    pub now_ns: u64,
}

impl Default for Gpio {
    fn default() -> Self { Self::new() }
}

impl Gpio {
    pub fn new() -> Self {
        let mut g = Gpio { fsel: [0; NPINS], out: 0, input: [None; NPINS], pull: [0; NPINS], eds: 0, ren: 0, fen: 0, hen: 0, len: 0, aren: 0, afen: 0,
                           last_level: 0, events: Vec::new(), now_ns: 0 };
        // reset pulls as on the Pi 4: GPIO0-8 up, 9-27 down
        for p in 0..28 { g.pull[p] = if p <= 8 { 1 } else { 2 }; }
        g.last_level = g.levels();
        g
    }

    fn is_output(&self, p: usize) -> bool { self.fsel[p] == 1 }

    /// Level of every line as the pins see it.
    pub fn levels(&self) -> u64 {
        let mut v = 0u64;
        for p in 0..NPINS {
            let l = if self.is_output(p) { self.out >> p & 1 != 0 } else { self.input[p].unwrap_or(self.pull[p] == 1) };
            if l { v |= 1 << p; }
        }
        v
    }

    pub fn level(&self, p: usize) -> bool { self.levels() >> p & 1 != 0 }

    /// Drive an input from outside (None releases the line to its pull).
    pub fn set_input(&mut self, p: usize, v: Option<bool>) {
        if p < NPINS { self.input[p] = v; self.update(); }
    }

    /// Recompute levels: record output changes and latch edge events.
    fn update(&mut self) {
        let now = self.levels();
        let changed = now ^ self.last_level;
        if changed != 0 {
            let rise = changed & now;
            let fall = changed & !now;
            self.eds |= (rise & (self.ren | self.aren)) | (fall & (self.fen | self.afen));
            for p in 0..NPINS {
                if changed >> p & 1 != 0 && self.is_output(p) {
                    self.events.push(PinEvent { pin: p as u8, level: now >> p & 1 != 0, time_ns: self.now_ns });
                }
            }
            if self.events.len() > 4096 { self.events.drain(..2048); }
        }
        // level detects are continuously asserted while the level holds
        self.eds |= (now & self.hen) | (!now & self.len & ((1u64 << NPINS) - 1));
        self.last_level = now;
    }

    /// Bank interrupt lines (0..27, 28..45, 46..57).
    pub fn irq(&self) -> [bool; 3] {
        let e = self.eds;
        [e & 0x0fff_ffff != 0, e >> 28 & 0x3_ffff != 0, e >> 46 & 0xfff != 0]
    }

    fn word(v: u64, hi: bool) -> u32 { if hi { (v >> 32) as u32 } else { v as u32 } }
    fn set_word(v: &mut u64, hi: bool, w: u32) { if hi { *v = (*v & 0xffff_ffff) | ((w as u64) << 32) } else { *v = (*v & !0xffff_ffff) | w as u64 } }

    pub fn read(&mut self, off: u64) -> u32 {
        let o = off as usize;
        let hi = o & 4 != 0;
        match o {
            0x00..=0x14 => {
                let base = o / 4 * 10;
                (0..10).filter(|k| base + k < NPINS).fold(0, |acc, k| acc | (self.fsel[base + k] as u32) << (3 * k))
            }
            0x34 | 0x38 => Self::word(self.levels(), o == 0x38),
            0x40 | 0x44 => Self::word(self.eds, o == 0x44),
            0x4c | 0x50 => Self::word(self.ren, o == 0x50),
            0x58 | 0x5c => Self::word(self.fen, o == 0x5c),
            0x64 | 0x68 => Self::word(self.hen, o == 0x68),
            0x70 | 0x74 => Self::word(self.len, o == 0x74),
            0x7c | 0x80 => Self::word(self.aren, o == 0x80),
            0x88 | 0x8c => Self::word(self.afen, o == 0x8c),
            0xe4..=0xf0 => {
                let base = (o - 0xe4) / 4 * 16;
                (0..16).filter(|k| base + k < NPINS).fold(0, |acc, k| acc | (self.pull[base + k] as u32) << (2 * k))
            }
            _ => { let _ = hi; 0 }
        }
    }

    pub fn write(&mut self, off: u64, v: u32) {
        let o = off as usize;
        match o {
            0x00..=0x14 => { let base = o / 4 * 10; for k in 0..10 { if base + k < NPINS { self.fsel[base + k] = ((v >> (3 * k)) & 7) as u8; } } }
            0x1c | 0x20 => { let m = Self::word(u64::MAX, o == 0x20) & v; let mut w = Self::word(self.out, o == 0x20); w |= m; Self::set_word(&mut self.out, o == 0x20, w); }
            0x28 | 0x2c => { let mut w = Self::word(self.out, o == 0x2c); w &= !v; Self::set_word(&mut self.out, o == 0x2c, w); }
            0x40 | 0x44 => { let mut w = Self::word(self.eds, o == 0x44); w &= !v; Self::set_word(&mut self.eds, o == 0x44, w); }
            0x4c | 0x50 => Self::set_word(&mut self.ren, o == 0x50, v),
            0x58 | 0x5c => Self::set_word(&mut self.fen, o == 0x5c, v),
            0x64 | 0x68 => Self::set_word(&mut self.hen, o == 0x68, v),
            0x70 | 0x74 => Self::set_word(&mut self.len, o == 0x74, v),
            0x7c | 0x80 => Self::set_word(&mut self.aren, o == 0x80, v),
            0x88 | 0x8c => Self::set_word(&mut self.afen, o == 0x8c, v),
            0xe4..=0xf0 => { let base = (o - 0xe4) / 4 * 16; for k in 0..16 { if base + k < NPINS { self.pull[base + k] = ((v >> (2 * k)) & 3) as u8; } } }
            _ => {}
        }
        self.update();
    }

    pub fn save_state(&self, w: &mut emu_core::snap::W) {
        w.tag("gpio-v1");
        w.bytes(&self.fsel); w.bytes(&self.pull);
        for v in [self.out, self.eds, self.ren, self.fen, self.hen, self.len, self.aren, self.afen, self.last_level] { w.u64(v); }
        for p in self.input { w.u8(match p { None => 2, Some(true) => 1, Some(false) => 0 }); }
    }
    pub fn load_state(&mut self, r: &mut emu_core::snap::R) -> Result<(), emu_core::snap::SnapError> {
        r.tag("gpio-v1")?;
        self.fsel.copy_from_slice(&r.bytes()?[..NPINS]); self.pull.copy_from_slice(&r.bytes()?[..NPINS]);
        for v in [&mut self.out, &mut self.eds, &mut self.ren, &mut self.fen, &mut self.hen, &mut self.len, &mut self.aren, &mut self.afen, &mut self.last_level] { *v = r.u64()?; }
        for p in self.input.iter_mut() { *p = match r.u8()? { 2 => None, 1 => Some(true), _ => Some(false) }; }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn output_events_and_input_edges() {
        let mut g = Gpio::new();
        g.write(0x04, 1 << 21); // GPIO17 output
        g.write(0x1c, 1 << 17); // set
        g.write(0x28, 1 << 17); // clear
        assert_eq!(g.events.iter().map(|e| (e.pin, e.level)).collect::<Vec<_>>(), vec![(17, true), (17, false)]);
        g.write(0x4c, 1 << 27); // rising edge on GPIO27
        assert!(!g.level(27)); // pulled down
        g.set_input(27, Some(true));
        assert_eq!(g.read(0x40) >> 27 & 1, 1);
        assert!(g.irq()[0]);
        g.write(0x40, 1 << 27);
        assert!(!g.irq()[0]);
    }
}
