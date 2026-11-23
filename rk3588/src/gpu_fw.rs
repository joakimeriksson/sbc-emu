//! Host model of the CSF global management interface (not MCU instruction execution).
//! Queues retain their registers and setup across doorbells and ring wraps.
use crate::gpu_mmu::{AddressSpace, Fault};
#[derive(Default)]
struct Queue {
    binding: [u64; 4], // Ring base/size and VM page-table configuration.
    stream: crate::gpu_cs::Stream,
}
const SHARED: u64 = 0x04000000;
#[derive(Default)]
pub struct Firmware {
    pub running: bool,
    input: u64,
    output: u64,
    groups: u32,
    stride: u64,
    failed_queues: std::collections::BTreeSet<(u32, u32)>,
    queues: std::collections::BTreeMap<(u32, u32), Queue>,
    tiler: crate::gpu_tiler::Tiler,
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
        self.failed_queues.clear();
        self.queues.clear();
        self.tiler = crate::gpu_tiler::Tiler::default();
        let input = read(m, ram, SHARED + 8)? as u64;
        let output = read(m, ram, SHARED + 12)? as u64;
        let groups = read(m, ram, SHARED + 16)?;
        let stride = read(m, ram, SHARED + 20)? as u64;
        if !(3..=31).contains(&groups)
            || stride < 32
            || !(SHARED..SHARED + 0x4000000).contains(&input)
            || !(SHARED..SHARED + 0x4000000).contains(&output)
        {
            return Err(Fault::UnsupportedConfiguration);
        }
        // Validate the firmware-provided layout before publishing a boot-complete event.
        for g in 0..groups as u64 {
            let c = SHARED + 0x1000 + g * stride;
            write(m, ram, c + 12, 4096)?;
            write(m, ram, c + 16, 4096)?;
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
                write(m, ram, cs, 255 | (8 << 8) | (3 << 16))?;
            }
        }
        read(m, ram, input)?;
        write(m, ram, output, 0)?;
        // Implemented protocol version, independent of the binary's build version.
        write(m, ram, SHARED, 0x01000000)?;
        self.groups = groups;
        self.stride = stride;
        self.input = input;
        self.output = output;
        self.running = true;
        Ok(())
    }
    pub fn queues(&mut self, spaces: &[AddressSpace; 16], ram: &mut [u8]) -> Result<u32, Fault> {
        if !self.running {
            return Ok(0);
        }
        let m = &spaces[0];
        let mut irq = 0;
        for g in 0..self.groups {
            let ctrl = SHARED + 0x1000 + g as u64 * self.stride;
            let input = read(m, ram, ctrl + 4)? as u64;
            let output = read(m, ram, ctrl + 8)? as u64;
            let req = read(m, ram, input)?;
            let old = read(m, ram, output)?;
            let ack = (old & !0x37) | (req & 0x37);
            write(m, ram, output, ack)?;
            if (old ^ ack) & read(m, ram, input + 4)? != 0 {
                irq |= 1 << g;
            }
            if req & 7 == 0 {
                self.failed_queues.retain(|&(group, _)| group != g);
                self.queues.retain(|&(group, _), _| group != g);
                self.tiler.clear_group(g);
            }
            if std::env::var_os("SBC_MALI_DEBUG").is_some() {
                eprintln!("[mali fw] group {g} req {req:#x} ack {ack:#x}");
            }
            if req & 7 != 1 && req & 7 != 3 {
                continue;
            }
            let asid = read(m, ram, input + 80)? as usize;
            if asid == 0 || asid >= 16 {
                return Err(Fault::UnsupportedConfiguration);
            }
            let count = read(m, ram, ctrl + 20)?;
            let stride = read(m, ram, ctrl + 24)? as u64;
            if count > 32 || stride < 12 || stride > 4096 {
                return Err(Fault::UnsupportedConfiguration);
            }
            let mut completed = false;
            let mut failed = false;
            for q in 0..count {
                let mut queue_completed = false;
                let cs = ctrl + 0x40 + q as u64 * stride;
                let ci = read(m, ram, cs + 4)? as u64;
                let co = read(m, ram, cs + 8)? as u64;
                let cr = read(m, ram, ci)?;
                let previous = read(m, ram, co)?;
                write(m, ram, co, (previous & !0xf17) | (cr & 0xf17))?;
                if cr & 7 != 1 {
                    if cr & 7 == 0 {
                        self.failed_queues.remove(&(g, q));
                        self.queues.remove(&(g, q));
                        self.tiler.clear_queue((g, q));
                    }
                    continue;
                }
                if self.failed_queues.contains(&(g, q)) {
                    continue;
                }
                let r64 = |r: &[u8], a: u64| -> Result<u64, Fault> {
                    Ok(read(m, r, a)? as u64 | ((read(m, r, a + 4)? as u64) << 32))
                };
                let base = r64(ram, ci + 16)?;
                let size = read(m, ram, ci + 24)? as u64;
                let ri = r64(ram, ci + 48)?;
                let ro = r64(ram, ci + 56)?;
                let insert = r64(ram, ri)?;
                let mut extract = r64(ram, ro)?;
                if std::env::var_os("SBC_MALI_DEBUG").is_some() {
                    eprintln!("[mali fw] group {g} queue {q} req {cr:#x} extract {extract:#x} insert {insert:#x}");
                }
                if size == 0
                    || size > 1 << 20
                    || !size.is_power_of_two()
                    || insert < extract
                    || insert - extract > size
                    || insert & 7 != 0
                    || extract & 7 != 0
                {
                    return Err(Fault::UnsupportedConfiguration);
                }
                let binding = [base, size, spaces[asid].transtab, spaces[asid].transcfg];
                while extract < insert {
                    let offset = extract % size;
                    let len = (insert - extract).min(size - offset);
                    let addr = base.checked_add(offset).ok_or(Fault::AddressSize)?;
                    // Keep the old binding until STOP/TERMINATE, including
                    // after a rejected rebind, so a retry cannot bypass it.
                    if self
                        .queues
                        .get(&(g, q))
                        .is_some_and(|queue| queue.binding != binding)
                    {
                        return Err(Fault::UnsupportedConfiguration);
                    }
                    let mut stream = if let Some(mut queue) = self.queues.remove(&(g, q)) {
                        queue
                            .stream
                            .next_segment(addr, len)
                            .map_err(|_| Fault::UnsupportedConfiguration)?;
                        queue.stream
                    } else {
                        crate::gpu_cs::Stream::new(addr, len)
                            .map_err(|_| Fault::UnsupportedConfiguration)?
                    };
                    let stop =
                        stream.run_with_tiler(&spaces[asid], ram, 65536, &mut self.tiler, (g, q));
                    if stop != crate::gpu_cs::Stop::Complete {
                        eprintln!("[mali] queue stopped: {:?}", stop);
                        // Report CS_FATAL, never signal success for unsupported work.
                        write(m, ram, co + 132, 0x40)?;
                        raise(m, ram, ci, co, 1 << 30)?;
                        self.failed_queues.insert((g, q));
                        failed = true;
                        raise(m, ram, input + 12, output + 12, 1 << q)?;
                        break;
                    }
                    self.queues.insert((g, q), Queue { binding, stream });
                    extract += len;
                    m.write(ram, ro, &extract.to_le_bytes(), false)?;
                    completed = true;
                    queue_completed = true;
                }
                if queue_completed {
                    raise(m, ram, input + 12, output + 12, 1 << q)?;
                }
            }
            if failed {
                irq |= 1 << g;
            }
            let db = read(m, ram, input + 8)?;
            write(m, ram, output + 8, db)?;
            if completed {
                // CSG_SYNC_UPDATE
                raise(m, ram, input, output, 1 << 28)?;
                irq |= 1 << g;
            }
        }
        let db = read(m, ram, self.input + 8)?;
        write(m, ram, self.output + 8, db)?;
        Ok(irq)
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

emu_core::snap_struct!(Queue { binding, stream });
emu_core::snap_struct!(Firmware { running, input, output, groups, stride, failed_queues, queues, tiler });

/// Raise a toggle-style event (bit `bit` of the word at `event`, acknowledged by the host
/// copying it into the word at `ack`): the event is pending while the two differ. An event
/// still pending stays pending; toggling it again would withdraw it before the host saw it.
fn raise(m: &AddressSpace, ram: &mut [u8], ack: u64, event: u64, bit: u32) -> Result<(), Fault> {
    let a = read(m, ram, ack)?;
    let e = read(m, ram, event)?;
    write(m, ram, event, (e & !bit) | (!a & bit))
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
    fn queue_fixture() -> (Firmware, [AddressSpace; 16], Vec<u8>) {
        let (m, mut r) = fixture();
        let mut f = Firmware::default();
        f.boot(&m, &mut r).unwrap();
        f.groups = 1;
        let gi = SHARED + 0x3000;
        write(&m, &mut r, gi, 1).unwrap();
        write(&m, &mut r, gi + 80, 1).unwrap();
        for q in 0..8u64 {
            let ctrl = SHARED + 0x1040 + q * 12;
            write(&m, &mut r, ctrl + 4, (SHARED + 0x4000 + q * 128) as u32).unwrap();
            write(&m, &mut r, ctrl + 8, (SHARED + 0x8000 + q * 256) as u32).unwrap();
        }
        let ci = SHARED + 0x4000;
        write(&m, &mut r, ci, 1).unwrap();
        for (off, v) in [
            (16, 0x18000),
            (24, 4096),
            (48, SHARED + 0x5000),
            (56, SHARED + 0xa000),
        ] {
            m.write(&mut r, ci + off, &v.to_le_bytes(), false).unwrap();
        }
        m.write(&mut r, SHARED + 0x5000, &8u64.to_le_bytes(), false)
            .unwrap();
        let mut spaces = [m; 16];
        spaces[1] = AddressSpace {
            transtab: 0,
            transcfg: 2,
        };
        (f, spaces, r)
    }
    #[test]
    fn queues_advance_only_completed_work_and_interrupt_on_fault() {
        for (opcode, done) in [(0u64, true), (4, false)] {
            let (mut f, spaces, mut r) = queue_fixture();
            let m = spaces[0];
            r[0x18000..0x18008].copy_from_slice(&(opcode << 56).to_le_bytes());
            assert_eq!(f.queues(&spaces, &mut r).unwrap() & 1, 1);
            assert_eq!(
                read(&m, &r, SHARED + 0xa000).unwrap(),
                if done { 8 } else { 0 }
            );
            assert_eq!(
                read(&m, &r, SHARED + 0x8000).unwrap() & (1 << 30) != 0,
                !done
            );
            if !done {
                let fatal = read(&m, &r, SHARED + 0x8000).unwrap();
                assert_eq!(f.queues(&spaces, &mut r).unwrap(), 0);
                assert_eq!(read(&m, &r, SHARED + 0x8000).unwrap(), fatal);
                assert_eq!(read(&m, &r, SHARED + 0xa000).unwrap(), 0);
            }
        }
    }
    #[test]
    fn queue_context_survives_doorbells_and_wrap_but_not_stop_or_terminate() {
        let (mut f, spaces, mut r) = queue_fixture();
        let m = spaces[0];
        let ci = SHARED + 0x4000;
        let put64 =
            |r: &mut [u8], a, value: u64| m.write(r, a, &value.to_le_bytes(), false).unwrap();
        write(&m, &mut r, ci + 24, 64).unwrap();
        put64(&mut r, SHARED + 0xa000, 56);
        put64(&mut r, SHARED + 0x5000, 64);
        r[0x18038..0x18040].copy_from_slice(&((1u64 << 56) | 0x1b000).to_le_bytes());
        f.queues(&spaces, &mut r).unwrap();
        assert_eq!(f.queues[&(0, 0)].stream.regs[0], 0x1b000);
        // The next doorbell wraps the ring and uses the address set above.
        r[0x18000..0x18008].copy_from_slice(&((2u64 << 56) | (4 << 48) | 37).to_le_bytes());
        r[0x18008..0x18010].copy_from_slice(&((21u64 << 56) | (4 << 48) | (1 << 16)).to_le_bytes());
        put64(&mut r, SHARED + 0x5000, 80);
        f.queues(&spaces, &mut r).unwrap();
        assert_eq!(&r[0x1b000..0x1b004], &37u32.to_le_bytes());
        assert_eq!(read(&m, &r, SHARED + 0xa000).unwrap(), 80);
        // A changed ring cannot inherit the old registers, even on retries.
        write(&m, &mut r, ci + 16, 0x19000).unwrap();
        put64(&mut r, SHARED + 0x5000, 88);
        for _ in 0..2 {
            assert_eq!(
                f.queues(&spaces, &mut r),
                Err(Fault::UnsupportedConfiguration)
            );
            assert_eq!(f.queues[&(0, 0)].stream.regs[0], 0x1b000);
        }
        write(&m, &mut r, ci, 0).unwrap();
        f.queues(&spaces, &mut r).unwrap();
        assert!(f.queues.is_empty());
        write(&m, &mut r, ci, 1).unwrap();
        f.queues(&spaces, &mut r).unwrap();
        assert_eq!(f.queues[&(0, 0)].stream.regs[0], 0);
        write(&m, &mut r, SHARED + 0x3000, 0).unwrap();
        f.queues(&spaces, &mut r).unwrap();
        assert!(f.queues.is_empty());
    }
    #[test]
    fn a_second_completion_keeps_an_unacknowledged_sync_update_pending() {
        // Two jobs complete before the kernel acknowledges the first CSG_SYNC_UPDATE:
        // toggling twice used to withdraw the event, and the kernel timed the job out.
        let m = AddressSpace { transtab: 0, transcfg: 2 };
        let mut ram = vec![0u8; 64];
        let (req, ack) = (0u64, 8u64);
        raise(&m, &mut ram, req, ack, 1 << 28).unwrap();
        assert_ne!(read(&m, &ram, ack).unwrap() & (1 << 28), read(&m, &ram, req).unwrap() & (1 << 28));
        raise(&m, &mut ram, req, ack, 1 << 28).unwrap();
        assert_ne!(read(&m, &ram, ack).unwrap() & (1 << 28), read(&m, &ram, req).unwrap() & (1 << 28));
        // The kernel acknowledges by copying the bit; the next completion raises it again.
        let a = read(&m, &ram, ack).unwrap();
        write(&m, &mut ram, req, a).unwrap();
        raise(&m, &mut ram, req, ack, 1 << 28).unwrap();
        assert_ne!(read(&m, &ram, ack).unwrap() & (1 << 28), read(&m, &ram, req).unwrap() & (1 << 28));
    }
}
