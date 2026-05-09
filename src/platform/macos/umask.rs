use libc::mode_t;
 
pub struct UmaskGuard {
    previous: mode_t,
}
 
impl Drop for UmaskGuard {
    fn drop(&mut self) {
        // SAFETY: umask is async-signal-safe and always succeeds.
        unsafe { libc::umask(self.previous); }
    }
}
 
pub fn tighten(new_mask: mode_t) -> UmaskGuard {
    // SAFETY: umask is process-global; we capture the previous value so we
    // can restore it on drop. Single-threaded at startup so no race.
    let previous = unsafe { libc::umask(new_mask) };
    UmaskGuard { previous }
}