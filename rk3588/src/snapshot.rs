//! Snapshots of the whole machine: cores, devices, virtual time, what was written to the disks,
//! and the RAM pages that are not zero. Restored into a machine built with the same options, the
//! guest goes on as if nothing had happened; on one thread, interpreted, to the instruction.
//!
//! Not in a snapshot: the disk images (the same must be attached), compiled code, host sockets
//! behind NAT, and recorded audio not yet collected.
use crate::{CoreState, Machine, Soc};
use emu_core::snap::{self, Snap, SnapError, R, W};

const BOARD: &str = "orangepi5";
/// Of the layout: raise it when what is saved changes.
const VERSION: u32 = 3;

emu_core::snap_enum!(CoreState { Off, Run, Wfi, Wfe });

impl Default for CoreState {
    fn default() -> Self { CoreState::Off }
}

impl Snap for Soc {
    fn save(&self, w: &mut W) {
        let Self { ram, uart, mmc, gpu, gmac, net, gic, display, xhci, pcie, usb, audio, grf, vop2, scmi, now, hdmi, hdptx, next_net_poll, dirty, unknown } = self;
        snap::save_ram(w, ram);
        uart.save(w);
        mmc.save(w);
        gpu.save(w);
        gmac.save(w);
        snap::save_fitted(w, net);
        gic.save(w);
        display.save(w);
        xhci.save(w);
        pcie.save(w);
        usb.save(w);
        audio.save(w);
        grf.save(w);
        vop2.save(w);
        scmi.save(w);
        now.save(w);
        hdmi.save(w);
        snap::save_each(w, hdptx);
        next_net_poll.save(w);
        dirty.save(w);
        unknown.save(w);
    }
    fn load(&mut self, r: &mut R) -> Result<(), SnapError> {
        let Self { ram, uart, mmc, gpu, gmac, net, gic, display, xhci, pcie, usb, audio, grf, vop2, scmi, now, hdmi, hdptx, next_net_poll, dirty, unknown } = self;
        snap::load_ram(r, ram)?;
        uart.load(r)?;
        mmc.load(r)?;
        gpu.load(r)?;
        gmac.load(r)?;
        snap::load_fitted(r, net, "a network (--net)")?;
        gic.load(r)?;
        display.load(r)?;
        xhci.load(r)?;
        pcie.load(r)?;
        usb.load(r)?;
        audio.load(r)?;
        grf.load(r)?;
        vop2.load(r)?;
        scmi.load(r)?;
        now.load(r)?;
        hdmi.load(r)?;
        snap::load_each(r, hdptx, "HDPTX registers")?;
        next_net_poll.load(r)?;
        dirty.load(r)?;
        unknown.load(r)
    }
}

impl Machine {
    /// The machine as a snapshot file. An error when it has state that cannot be saved (a disk
    /// that writes through to its image).
    pub fn save(&self) -> Result<Vec<u8>, String> {
        snap::check_disk(self.soc.mmc.card.disk.as_deref())?;
        Ok(snap::pack(BOARD, VERSION, &self.state()))
    }

    /// The state in a snapshot, unpacked.
    pub fn state(&self) -> Vec<u8> {
        let Self {
            cpus, state, gpu_probe, dtb: _, soc, now, cur, next_check, round_end, multi: _,
            #[cfg(not(target_arch = "wasm32"))]
            mt: _,
            irqs, exceptions, breakpoints: _,
        } = self;
        let mut w = W::default();
        w.u32(cpus.len() as u32);
        // `gpu_probe` is in the device tree the guest was given: the same machine must have it
        gpu_probe.save(&mut w);
        now.save(&mut w);
        cur.save(&mut w);
        next_check.save(&mut w);
        round_end.save(&mut w);
        irqs.save(&mut w);
        exceptions.save(&mut w);
        snap::save_each(&mut w, state);
        for c in cpus { c.save_state(&mut w); }
        soc.save(&mut w);
        w.0
    }

    /// Continue from a snapshot made by `save`. After an error the machine is in pieces.
    pub fn restore(&mut self, file: &[u8]) -> Result<(), String> {
        let disks = [
            self.soc.mmc.card.disk.as_ref().map_or(0, |d| d.sectors()),
            self.soc.pcie.nvme.as_ref().and_then(|n| n.disk.as_ref()).map_or(0, |d| d.sectors()),
            self.soc.usb.storage.as_ref().map_or(0, |s| s.disk.sectors()),
        ];
        let limit = snap::limit(self.soc.ram.len(), disks.iter().sum());
        self.load_state(&snap::unpack(BOARD, VERSION, limit, file).map_err(|e| e.0)?).map_err(|e| e.0)
    }

    fn load_state(&mut self, state: &[u8]) -> Result<(), SnapError> {
        let mut r = R::new(state);
        let n = r.u32()? as usize;
        if n != self.cpus.len() {
            return Err(SnapError(format!("the snapshot's machine has {n} cores, this one {} (--smp {n})", self.cpus.len())));
        }
        if r.bool()? != self.gpu_probe { return Err(SnapError("the snapshot's machine was started with another --gpu".into())); }
        // the core threads start anew, at the restored time
        #[cfg(not(target_arch = "wasm32"))]
        { self.mt = None; }
        self.now.load(&mut r)?;
        let mut cur = 0usize;
        cur.load(&mut r)?;
        if cur >= n { return Err(SnapError(format!("core {cur} of {n}"))); }
        self.switch_to(cur); // it has the shared JIT
        self.next_check.load(&mut r)?;
        self.round_end.load(&mut r)?;
        self.irqs.load(&mut r)?;
        self.exceptions.load(&mut r)?;
        snap::load_each(&mut r, &mut self.state, "cores")?;
        for c in self.cpus.iter_mut() { c.load_state(&mut r)?; }
        self.soc.load(&mut r)?;
        if r.p != state.len() { return Err(SnapError(format!("{} bytes too many", state.len() - r.p))); }
        for c in self.cpus.iter_mut() { c.jit_invalidate(u64::MAX); }
        Ok(())
    }
}
