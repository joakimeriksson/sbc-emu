//! The I²C device side shared by every board's controller model: a device sees START
//! (with the direction), the bytes and STOP, whatever SoC's controller drives the bus.

/// A device on the bus. A transaction is `start`, then bytes, then `stop`; a repeated
/// start is another `start` without a `stop`.
pub trait I2cDevice {
    /// Addressed for a write (`read` false) or a read. Returns false for a NACK.
    fn start(&mut self, read: bool) -> bool {
        let _ = read;
        true
    }
    /// A byte written by the host. Returns false for a NACK.
    fn write(&mut self, byte: u8) -> bool;
    /// A byte read by the host.
    fn read(&mut self) -> u8;
    /// STOP condition.
    fn stop(&mut self) {}
    /// Virtual time in nanoseconds, given before each transaction (for clocks).
    fn clock(&mut self, now_ns: u64) {
        let _ = now_ns;
    }
    /// The concrete device, for a machine that inspects its state (e.g. a PMIC's power-off).
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        None
    }
}

/// The register protocol most I²C chips share: the first byte of a write transaction
/// selects the register, later bytes write it and move on to the next, and reads continue
/// from the selected register.
#[derive(Default)]
pub struct RegisterPointer {
    pub pointer: u8,
    expect_pointer: bool,
}

impl RegisterPointer {
    pub fn start(&mut self, read: bool) {
        self.expect_pointer = !read;
    }

    /// A byte written by the host: the register it writes, or None when it selected one.
    pub fn written(&mut self, byte: u8) -> Option<u8> {
        if self.expect_pointer {
            self.pointer = byte;
            self.expect_pointer = false;
            None
        } else {
            Some(self.next())
        }
    }

    /// The register the next byte comes from or goes to.
    pub fn next(&mut self) -> u8 {
        let reg = self.pointer;
        self.pointer = reg.wrapping_add(1);
        reg
    }
}

emu_core::snap_struct!(RegisterPointer { pointer, expect_pointer });
