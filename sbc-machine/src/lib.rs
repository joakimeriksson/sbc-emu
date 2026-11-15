//! Host-facing board selection and execution boundary.
//!
//! During migration, board adapters own their concrete machines and borrow this enum
//! for common operations. No facade dispatch occurs inside CPU or device execution.
//! Construction, reset, debug and optional devices remain in the Pi adapter for now.

mod board;
pub use board::Board;

// Both board implementations use the same neutral stop reasons.
pub use emu_core::Stop;

/// A borrowed host view; ownership and board-specific configuration stay with adapters.
pub enum Machine<'a> {
    Pi4(&'a mut bcm2711::Machine),
    OrangePi5(&'a mut rk3588::Machine),
}

impl Machine<'_> {
    /// Delegate the existing execution policy without changing scheduling.
    ///
    /// In deterministic mode `budget` bounds retired instructions and execution
    /// attempts (faults need not retire). In Pi threaded mode the existing runner
    /// uses a wall-clock slice instead; `budget` is not a strict threaded limit.
    /// Debug state may select deterministic execution even when threading is requested.
    pub fn run(&mut self, budget: u64) -> Stop {
        match self {
            Self::Pi4(m) => m.run(budget),
            Self::OrangePi5(m) => m.run(budget),
        }
    }

    /// Advance idle virtual time toward an absolute deadline in nanoseconds.
    /// Host pacing remains the caller's responsibility.
    pub fn idle_until(&mut self, time_ns: u64) {
        match self {
            Self::Pi4(m) => m.idle_until(time_ns),
            Self::OrangePi5(m) => m.idle_until(time_ns),
        }
    }

    /// Queue one byte, returning false without consuming it if the FIFO is full.
    /// The adapter must retain an unaccepted byte and retry after guest progress.
    pub fn serial_input(&mut self, byte: u8) -> bool {
        match self {
            Self::OrangePi5(m) => m.serial_input(byte),
            Self::Pi4(m) => {
                if !m.soc.uart.rx_room() {
                    return false;
                }
                m.soc.uart.push_rx(byte);
                m.soc.dirty = true;
                true
            }
        }
    }

    /// Inspect pending output without draining it (script matching before console flush).
    pub fn serial_output(&self) -> &[u8] {
        match self {
            Self::Pi4(m) => &m.soc.uart.out,
            Self::OrangePi5(m) => &m.soc.uart.out,
        }
    }

    pub fn take_serial_output(&mut self) -> Vec<u8> {
        match self {
            Self::Pi4(m) => std::mem::take(&mut m.soc.uart.out),
            Self::OrangePi5(m) => std::mem::take(&mut m.soc.uart.out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_backpressure_preserves_order_and_interrupt_notification() {
        let mut pi = bcm2711::Machine::new(65 << 20);
        pi.soc.dirty = false;
        for byte in 0..32 {
            assert!(Machine::Pi4(&mut pi).serial_input(byte));
        }
        assert!(pi.soc.dirty);
        assert!(!Machine::Pi4(&mut pi).serial_input(32));
        assert_eq!(pi.soc.uart.read(0), 0);
        assert!(Machine::Pi4(&mut pi).serial_input(32));
        for byte in 1..=32 {
            assert_eq!(pi.soc.uart.read(0), byte);
        }
        assert_ne!(pi.soc.uart.read(0x18) & (1 << 4), 0);
        pi.soc.uart.write(0, b'O' as u32);
        pi.soc.uart.write(0, b'K' as u32);
        let mut host = Machine::Pi4(&mut pi);
        assert_eq!(host.serial_output(), b"OK");
        assert_eq!(host.take_serial_output(), b"OK");
        assert!(host.serial_output().is_empty());
    }

    #[test]
    fn facade_preserves_execution_and_idle_state() {
        let mut direct = bcm2711::Machine::new(65 << 20);
        let mut wrapped = bcm2711::Machine::new(65 << 20);
        // ADD X0,X0,#1; WFI: exercise instruction progress and an idle deadline.
        for m in [&mut direct, &mut wrapped] {
            m.cpus[0].pc = 0x80000;
            m.soc.ram[0x80000..0x80008]
                .copy_from_slice(&[0x00, 0x04, 0x00, 0x91, 0x7f, 0x20, 0x03, 0xd5]);
        }
        assert_eq!(direct.run(1), Machine::Pi4(&mut wrapped).run(1));
        assert_eq!(direct.cpus[0].x[0], 1);
        let stop = direct.run(100);
        assert_eq!(stop, Machine::Pi4(&mut wrapped).run(100));
        let Stop::Idle(deadline) = stop else {
            panic!("expected idle, got {stop:?}");
        };
        direct.idle_until(deadline);
        Machine::Pi4(&mut wrapped).idle_until(deadline);
        assert_eq!(direct.save(), wrapped.save());
    }
}
