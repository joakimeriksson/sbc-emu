//! Host model of the CSF global management interface (not MCU instruction execution).
//! Queue execution is not implemented and must not be acknowledged as completed.
use crate::gpu_mmu::{AddressSpace, Fault};
const SHARED: u64 = 0x04000000;
#[derive(Default)]
pub struct Firmware {
    pub running: bool,
    input: u64,
    output: u64,
}
fn read(m: &AddressSpace, ram: &[u8], addr: u64) -> Result<u32, Fault> {
    let mut b = [0; 4];
    m.read(ram, addr, &mut b, false)?;
    Ok(u32::from_le_bytes(b))
}
fn write(m: &AddressSpace, ram: &mut [u8], addr: u64, v: u32) -> Result<(), Fault> {
    m.write(ram, addr, &v.to_le_bytes(), false)
}
impl Firmware {
    pub fn boot(&mut self, m: &AddressSpace, ram: &mut [u8]) -> Result<(), Fault> {
        self.running = false;
        let input = read(m, ram, SHARED + 8)? as u64;
        let output = read(m, ram, SHARED + 12)? as u64;
        let groups = read(m, ram, SHARED + 16)?;
        let stride = read(m, ram, SHARED + 20)? as u64;
        if !(3..=32).contains(&groups)
            || stride < 32
            || !(SHARED..SHARED + 0x4000000).contains(&input)
            || !(SHARED..SHARED + 0x4000000).contains(&output)
        {
            return Err(Fault::UnsupportedConfiguration);
        }
        // Validate the firmware-provided layout before publishing a boot-complete event.
        for g in 0..groups as u64 {
            let c = SHARED + 0x1000 + g * stride;
            let streams = read(m, ram, c + 20)?;
            let ss = read(m, ram, c + 24)? as u64;
            if !(8..=32).contains(&streams) || ss < 12 {
                return Err(Fault::UnsupportedConfiguration);
            }
            for ptr in [read(m, ram, c + 4)?, read(m, ram, c + 8)?] {
                read(m, ram, ptr as u64)?;
            }
            for s in 0..streams as u64 {
                let cs = c + 0x40 + s * ss;
                read(m, ram, cs + 8)?;
            }
        }
        read(m, ram, input)?;
        write(m, ram, output, 0)?;
        // Implemented protocol version, independent of the binary's build version.
        write(m, ram, SHARED, 0x01000000)?;
        self.input = input;
        self.output = output;
        self.running = true;
        Ok(())
    }
    /// Return (interrupt requested, halt requested). Only implemented global requests are acknowledged.
    pub fn doorbell(&mut self, m: &AddressSpace, ram: &mut [u8]) -> Result<(bool, bool), Fault> {
        if !self.running {
            return Ok((false, false));
        }
        let req = read(m, ram, self.input)?;
        let old = read(m, ram, self.output)?;
        let supported = (1 << 1) | (1 << 2) | (1 << 3) | (1 << 8) | (1 << 10);
        let ack = (old & !supported) | (req & supported);
        write(m, ram, self.output, ack)?;
        let irq = (old ^ ack) & read(m, ram, self.input + 4)? != 0;
        if req & 1 != 0 {
            self.running = false;
        }
        Ok((irq, req & 1 != 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (AddressSpace, Vec<u8>) {
        let mut r = vec![0; 0x20000];
        for (a, v) in [(0x1000, 0x2003u64), (0x2000, 0x3003), (0x3100, 0x4003)] {
            r[a..a + 8].copy_from_slice(&v.to_le_bytes());
        }
        for i in 0..16 {
            r[0x4000 + i * 8..0x4008 + i * 8]
                .copy_from_slice(&(0x8000u64 + i as u64 * 4096 + 0x443).to_le_bytes());
        }
        let m = AddressSpace {
            transtab: 0x1000,
            transcfg: 6 | (7 << 6),
        };
        for (off, v) in [
            (8, SHARED + 0x2000),
            (12, SHARED + 0x6000),
            (16, 3),
            (20, 160),
        ] {
            write(&m, &mut r, SHARED + off, v as u32).unwrap();
        }
        for g in 0..3 {
            let c = SHARED + 0x1000 + g * 160;
            for (off, v) in [
                (4, SHARED + 0x3000),
                (8, SHARED + 0x7000),
                (20, 8),
                (24, 12),
            ] {
                write(&m, &mut r, c + off, v as u32).unwrap();
            }
        }
        (m, r)
    }
    #[test]
    fn boot_ping_configuration_and_halt() {
        let (m, mut r) = fixture();
        let mut f = Firmware::default();
        f.boot(&m, &mut r).unwrap();
        assert!(f.running);
        assert_eq!(read(&m, &r, SHARED).unwrap(), 0x01000000);
        write(&m, &mut r, f.input, 0x10e).unwrap();
        write(&m, &mut r, f.input + 4, u32::MAX).unwrap();
        assert_eq!(f.doorbell(&m, &mut r).unwrap(), (true, false));
        assert_eq!(read(&m, &r, f.output).unwrap(), 0x10e);
        assert_eq!(f.doorbell(&m, &mut r).unwrap(), (false, false));
        // Unsupported protected-mode request must not be acknowledged.
        write(&m, &mut r, f.input, 0x11e).unwrap();
        f.doorbell(&m, &mut r).unwrap();
        assert_eq!(read(&m, &r, f.output).unwrap(), 0x10e);
        write(&m, &mut r, f.input, 1).unwrap();
        assert!(f.doorbell(&m, &mut r).unwrap().1);
        assert!(!f.running);
    }
    #[test]
    fn malformed_layout_cannot_publish_boot() {
        let (m, mut r) = fixture();
        write(&m, &mut r, SHARED + 16, u32::MAX).unwrap();
        let mut f = Firmware::default();
        assert!(f.boot(&m, &mut r).is_err());
        assert!(!f.running);
        assert_eq!(read(&m, &r, SHARED).unwrap(), 0);
    }
}
