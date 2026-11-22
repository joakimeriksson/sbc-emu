//! The Orange Pi 5's headphone codec: an Everest ES8388 on I2C6 (address 0x10), as Linux's
//! `es8328` driver sets it up. It masters the I2S1 clocks from its 12.288 MHz MCLK, so its
//! registers give the playback rate (MASTERMODE, DACCONTROL2's MCLK/LRCK ratio). The DAC's
//! path to the headphone jack (LOUT1/ROUT1) sets what reaches the host: the DAC mute and
//! digital volume, the DAC and output power, the output mixer's DAC switch and the output
//! volume. The ADC side is registers only (the microphones record silence).

use peripherals::i2c::{I2cDevice, RegisterPointer};

pub const ADDRESS: u8 = 0x10;
/// The MCLK the board gives the codec (the CRU's I2S1 MCLKOUT, 12.288 MHz).
pub const MCLK: u64 = 12_288_000;

const NREGS: usize = 0x35;
const CHIPPOWER: usize = 0x02;
const DACPOWER: usize = 0x04;
const MASTERMODE: usize = 0x08;
const ADCCONTROL5: usize = 0x0d;
const DACCONTROL2: usize = 0x18;
const DACCONTROL3: usize = 0x19;
const LDACVOL: usize = 0x1a;
const RDACVOL: usize = 0x1b;
const DACCONTROL17: usize = 0x27;
const DACCONTROL20: usize = 0x2a;
const LOUT1VOL: usize = 0x2e;
const ROUT1VOL: usize = 0x2f;

/// Power-on register values.
const RESET: [u8; NREGS] = [
    0x06, 0x5c, 0xc3, 0xfc, 0xc0, 0x00, 0x00, 0x7c, 0x80, 0x00, 0x00, 0x06, 0x00, 0x06, 0x30, 0x30, //
    0xc0, 0xc0, 0x38, 0xb0, 0x32, 0x06, 0x00, 0x00, 0x06, 0x30, 0xc0, 0xc0, 0x08, 0x06, 0x1f, 0xf7, //
    0xfd, 0xff, 0x1f, 0xf7, 0xfd, 0xff, 0x00, 0x38, 0x28, 0x28, 0x38, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0xaa, 0xaa,
];

/// MCLK / LRCK for each value of the ratio fields (ADCCONTROL5, DACCONTROL2).
const RATIOS: [u32; 25] = [128, 192, 256, 384, 512, 576, 768, 1024, 1152, 1408, 1536, 2112, 2304, 125, 136, 250, 272, 375, 500, 544, 750, 1000, 1088, 1496, 1500];

pub struct Es8388 {
    pub regs: [u8; NREGS],
    pointer: RegisterPointer,
}

impl Default for Es8388 {
    fn default() -> Self {
        Self { regs: RESET, pointer: RegisterPointer::default() }
    }
}

impl Es8388 {
    /// The codec generates the bit and frame clocks.
    pub fn master(&self) -> bool {
        self.regs[MASTERMODE] & 0x80 != 0
    }

    /// The DAC's frame rate from MCLK `mclk` Hz (MASTERMODE.MCLKDIV2 halves it).
    pub fn dac_rate(&self, mclk: u64) -> u32 {
        self.rate(mclk, DACCONTROL2)
    }

    /// The ADC's frame rate.
    pub fn adc_rate(&self, mclk: u64) -> u32 {
        self.rate(mclk, ADCCONTROL5)
    }

    fn rate(&self, mclk: u64, reg: usize) -> u32 {
        let mclk = if self.regs[MASTERMODE] & 0x40 != 0 { mclk / 2 } else { mclk };
        let ratio = RATIOS.get(usize::from(self.regs[reg] & 0x1f)).copied().unwrap_or(256);
        (mclk / u64::from(ratio)) as u32
    }

    /// What the headphone outputs (LOUT1, ROUT1) make of full-scale DAC input: 0 when a
    /// stage of the path is off or muted, else the digital and output volumes.
    pub fn headphone_gain(&self) -> [f32; 2] {
        let r = &self.regs;
        let db_to = |db: f32| 10f32.powf(db / 20.0);
        let on = r[CHIPPOWER] & 0x40 == 0 && r[DACCONTROL3] & 0x04 == 0;
        let chan = |dac_off: u8, out_on: u8, mixer: usize, dac_vol: usize, out_vol: usize| {
            if !on || r[DACPOWER] & dac_off != 0 || r[DACPOWER] & out_on == 0 || r[mixer] & 0x80 == 0 {
                return 0.0;
            }
            // digital: 0 dB down in 0.5 dB steps (0xc0: -96 dB, silent); output: -45 dB up in
            // 1.5 dB steps to +4.5 dB (0x1e: 0 dB)
            let dac = f32::from(r[dac_vol].min(0xc0)) * -0.5;
            let out = (f32::from(r[out_vol].min(0x21)) - 30.0) * 1.5;
            if r[dac_vol] >= 0xc0 { 0.0 } else { db_to(dac + out) }
        };
        [chan(0x80, 0x20, DACCONTROL17, LDACVOL, LOUT1VOL), chan(0x40, 0x10, DACCONTROL20, RDACVOL, ROUT1VOL)]
    }
}

impl I2cDevice for Es8388 {
    fn start(&mut self, read: bool) -> bool {
        self.pointer.start(read);
        true
    }
    fn write(&mut self, byte: u8) -> bool {
        if let Some(reg) = self.pointer.written(byte) {
            if let Some(r) = self.regs.get_mut(usize::from(reg)) {
                *r = byte;
            }
        }
        true
    }
    fn read(&mut self) -> u8 {
        self.regs.get(usize::from(self.pointer.next())).copied().unwrap_or(0)
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

emu_core::snap_struct!(Es8388 { regs, pointer });

#[cfg(test)]
mod tests {
    use super::*;

    fn set(c: &mut Es8388, reg: usize, v: u8) {
        c.start(false);
        c.write(reg as u8);
        c.write(v);
    }

    #[test]
    fn rate_from_the_ratio_register() {
        let mut c = Es8388::default();
        set(&mut c, MASTERMODE, 0x80);
        set(&mut c, DACCONTROL2, 2);
        assert!(c.master());
        assert_eq!(c.dac_rate(MCLK), 48_000);
        set(&mut c, DACCONTROL2, 6);
        assert_eq!(c.dac_rate(MCLK), 16_000);
        set(&mut c, MASTERMODE, 0xc0);
        set(&mut c, DACCONTROL2, 2);
        assert_eq!(c.dac_rate(2 * MCLK), 48_000, "MCLKDIV2");
    }

    /// Silent out of reset; Linux's playback path (DAPM powers DAC, mixer and output) at
    /// 0 dB plays at full scale, and the volumes and the mute apply.
    #[test]
    fn headphone_path() {
        let mut c = Es8388::default();
        assert_eq!(c.headphone_gain(), [0.0, 0.0]);
        set(&mut c, CHIPPOWER, 0);
        set(&mut c, DACPOWER, 0x30);
        set(&mut c, DACCONTROL17, 0xb8);
        set(&mut c, DACCONTROL20, 0xb8);
        set(&mut c, LDACVOL, 0);
        set(&mut c, RDACVOL, 12);
        set(&mut c, LOUT1VOL, 0x1e);
        set(&mut c, ROUT1VOL, 0x1e);
        let [l, r] = c.headphone_gain();
        assert!((l - 1.0).abs() < 1e-6);
        assert!((r - 0.5012).abs() < 1e-3, "-6 dB: {r}");
        set(&mut c, DACCONTROL3, 0x04);
        assert_eq!(c.headphone_gain(), [0.0, 0.0], "muted");
        // reads continue from the selected register
        c.start(false);
        c.write(LOUT1VOL as u8);
        c.start(true);
        assert_eq!((c.read(), c.read()), (0x1e, 0x1e));
    }
}
