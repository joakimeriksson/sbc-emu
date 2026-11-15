//! Host-visible machine stop reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// instruction budget for this call used up
    Budget,
    /// idle: core waits and nothing is due before the returned time (ns)
    Idle(u64),
    Unimplemented(u64, u32),
    Hlt(u16),
    Brk(u16),
    /// PSCI SYSTEM_OFF
    PowerOff,
    /// PSCI SYSTEM_RESET
    Reset,
    /// a debugger breakpoint: core `k` is about to execute an instruction at a breakpoint
    Breakpoint(usize),
    /// a debugger watchpoint: core `k` just made an access (kind 1 write, 2 read) to the
    /// watched address
    Watchpoint(usize, u64, u8),
    /// the core keeps taking exceptions without retiring instructions (a fault loop)
    ExceptionStorm { pc: u64, esr: u64, elr: u64, far: u64, el: u8 },
}
