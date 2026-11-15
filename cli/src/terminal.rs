/// Restores the terminal mode on drop.
pub(crate) struct RawTty(pub(crate) bool);
impl RawTty {
    pub(crate) fn enable() -> Self {
        let tty = std::process::Command::new("stty").args(["raw", "-echo"]).stdin(std::process::Stdio::inherit()).status().is_ok_and(|s| s.success());
        RawTty(tty)
    }
}
impl Drop for RawTty {
    fn drop(&mut self) {
        if self.0 { let _ = std::process::Command::new("stty").arg("sane").stdin(std::process::Stdio::inherit()).status(); }
    }
}
